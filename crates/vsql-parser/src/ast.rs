//! VelocitySQL's own abstract syntax tree.
//!
//! # Why a private AST?
//!
//! Parsing is done with `sqlparser-rs` in PostgreSQL dialect mode, which gives
//! us breadth of syntax for free. But its AST is deliberately dialect-neutral:
//! it carries MySQL/Hive/Snowflake variants, `Top` clauses, optimizer hints and
//! dozens of fields VelocitySQL will never honour.
//!
//! Lowering that AST into the (much smaller) tree below buys three things:
//!
//! 1. **A closed world.** Every `Expr` variant here has a defined meaning. If a
//!    construct is not modelled, the user gets an explicit
//!    `feature not supported` error at parse time instead of a silently ignored
//!    clause at execution time.
//! 2. **Identifier normalisation happens once.** Unquoted identifiers are folded
//!    to lower case exactly like PostgreSQL does, and quoting is discarded;
//!    everything downstream can compare `String`s.
//! 3. **A stable internal contract.** Swapping the parser (or adding a second
//!    front-end, e.g. for the binary protocol) cannot ripple into the planner.
//!
//! All names are stored as `Vec<String>` so that schema-qualified and
//! table-qualified references can be represented uniformly.

use serde::{Deserialize, Serialize};
use vsql_types::{DataType, Value};

/// A possibly qualified object name, e.g. `public.users` → `["public", "users"]`.
pub type Name = Vec<String>;

/// A parsed statement, already normalised for the rest of the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    /// `SELECT` / `VALUES` / set operations.
    Query(Box<Query>),
    /// `INSERT`.
    Insert(Box<Insert>),
    /// `UPDATE`.
    Update(Box<Update>),
    /// `DELETE`.
    Delete(Box<Delete>),
    /// `CREATE TABLE`.
    CreateTable(Box<CreateTable>),
    /// `ALTER TABLE`.
    AlterTable(Box<AlterTable>),
    /// `DROP TABLE`.
    DropTable {
        /// `IF EXISTS` was given.
        if_exists: bool,
        /// Tables to drop, in statement order.
        names: Vec<Name>,
    },
    /// `CREATE SCHEMA`.
    CreateSchema {
        /// Schema name.
        name: String,
        /// `IF NOT EXISTS` was given.
        if_not_exists: bool,
    },
    /// `CREATE INDEX`.
    CreateIndex(Box<CreateIndex>),
    /// `CREATE VIEW`.
    CreateView(Box<CreateView>),
    /// `CREATE ROLE` / `CREATE USER`.
    CreateRole {
        /// Role name.
        name: String,
        /// `IF NOT EXISTS` was given.
        if_not_exists: bool,
        /// Attributes from the statement.
        options: Vec<RoleOption>,
    },
    /// `ALTER ROLE` / `ALTER USER`.
    AlterRole {
        /// Role name.
        name: String,
        /// Attributes to change.
        options: Vec<RoleOption>,
    },
    /// `DROP ROLE` / `DROP USER`.
    DropRole {
        /// `IF EXISTS` was given.
        if_exists: bool,
        /// Roles to drop.
        names: Vec<String>,
    },
    /// `CREATE DATABASE`.
    ///
    /// A database is the layer *above* a schema: it owns its own catalog and its
    /// own rows, so it is addressed by name on connect rather than by a `FROM`
    /// clause.
    CreateDatabase {
        /// Database name.
        name: String,
        /// `IF NOT EXISTS` was given.
        if_not_exists: bool,
    },
    /// `DROP DATABASE`.
    DropDatabase {
        /// `IF EXISTS` was given.
        if_exists: bool,
        /// Databases to drop.
        names: Vec<String>,
    },
    /// `TRUNCATE TABLE`.
    Truncate {
        /// Tables to truncate.
        names: Vec<Name>,
        /// `IF EXISTS` was given.
        if_exists: bool,
    },
    /// `BEGIN` / `START TRANSACTION`.
    StartTransaction {
        /// Requested isolation level, if any.
        isolation: Option<IsolationLevel>,
        /// `READ ONLY` / `READ WRITE`, if given.
        read_only: Option<bool>,
    },
    /// `COMMIT`.
    Commit,
    /// `ROLLBACK`, optionally `TO SAVEPOINT name`.
    Rollback {
        /// Savepoint to roll back to.
        savepoint: Option<String>,
    },
    /// `SAVEPOINT name`.
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] name`.
    ReleaseSavepoint(String),
    /// `EXPLAIN [ANALYZE] [VERBOSE] <statement>`.
    Explain {
        /// `ANALYZE` was given: execute the statement and report timings.
        analyze: bool,
        /// `VERBOSE` was given: show the full plan tree with expressions.
        verbose: bool,
        /// The statement being explained.
        statement: Box<Statement>,
    },
    /// `SHOW <name>`.
    Show(String),
    /// `SET <name> = <value>`.
    Set {
        /// Setting name (normalised to lower case).
        name: String,
        /// New value, if any.
        value: Option<Expr>,
    },
}

/// Transaction isolation levels supported by VelocitySQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationLevel {
    /// `READ UNCOMMITTED` (treated as `READ COMMITTED`).
    ReadUncommitted,
    /// `READ COMMITTED`.
    ReadCommitted,
    /// `REPEATABLE READ`.
    RepeatableRead,
    /// `SERIALIZABLE`.
    Serializable,
}

/// A complete query expression: `WITH` + body + `ORDER BY` + `LIMIT`/`OFFSET`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Query {
    /// Optional `WITH` clause.
    pub with: Option<With>,
    /// The query body.
    pub body: SetExpr,
    /// `ORDER BY` expressions.
    pub order_by: Vec<OrderByExpr>,
    /// `LIMIT` expression.
    pub limit: Option<Expr>,
    /// `OFFSET` expression.
    pub offset: Option<Expr>,
    /// `FOR UPDATE` / `FOR SHARE` was given.
    pub for_update: bool,
}

/// A `WITH` clause.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct With {
    /// `WITH RECURSIVE`.
    pub recursive: bool,
    /// The common table expressions, in declaration order.
    pub ctes: Vec<Cte>,
}

/// A single common table expression.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cte {
    /// CTE name.
    pub name: String,
    /// Optional explicit column names.
    pub columns: Vec<String>,
    /// The CTE body.
    pub query: Query,
}

/// The body of a query: a `SELECT`, a set operation, or a `VALUES` list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SetExpr {
    /// A single `SELECT`.
    Select(Box<Select>),
    /// A parenthesised query.
    Query(Box<Query>),
    /// `VALUES (...), (...)`.
    Values(Vec<Vec<Expr>>),
    /// `UNION` / `INTERSECT` / `EXCEPT`.
    SetOperation {
        /// Which set operator.
        op: SetOp,
        /// `ALL` was given (otherwise duplicates are removed).
        all: bool,
        /// Left input.
        left: Box<SetExpr>,
        /// Right input.
        right: Box<SetExpr>,
    },
}

/// The set operators of SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetOp {
    /// `UNION`
    Union,
    /// `INTERSECT`
    Intersect,
    /// `EXCEPT`
    Except,
}

/// A single `SELECT` block (without `ORDER BY`/`LIMIT`, which live on `Query`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Select {
    /// `DISTINCT` / `DISTINCT ON`.
    pub distinct: Option<Distinct>,
    /// The projection list.
    pub projection: Vec<SelectItem>,
    /// The `FROM` list; empty for `SELECT 1`.
    pub from: Vec<TableWithJoins>,
    /// The `WHERE` predicate.
    pub selection: Option<Expr>,
    /// `GROUP BY` expressions.
    pub group_by: Vec<Expr>,
    /// The `HAVING` predicate.
    pub having: Option<Expr>,
}

/// `DISTINCT` variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Distinct {
    /// `SELECT DISTINCT`.
    On,
    /// `SELECT DISTINCT ON (exprs)`.
    OnExprs(Vec<Expr>),
}

/// One element of a projection list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SelectItem {
    /// An expression with an optional output alias.
    Expr {
        /// The projected expression.
        expr: Expr,
        /// `AS alias`, if given.
        alias: Option<String>,
    },
    /// `*`.
    Wildcard,
    /// `alias.*`.
    QualifiedWildcard(String),
}

/// A `FROM` element followed by its joins.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableWithJoins {
    /// The left-most relation.
    pub relation: TableFactor,
    /// Joins applied left-to-right.
    pub joins: Vec<Join>,
}

/// Something that can appear in a `FROM` clause.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TableFactor {
    /// A named table, e.g. `public.users u`.
    Table {
        /// Qualified name.
        name: Name,
        /// Optional alias.
        alias: Option<TableAlias>,
    },
    /// A subquery in `FROM`.
    Derived {
        /// The subquery.
        subquery: Box<Query>,
        /// Optional alias.
        alias: Option<TableAlias>,
        /// `LATERAL` was given (only meaningful for correlated subqueries).
        lateral: bool,
    },
    /// A parenthesised join tree.
    NestedJoin {
        /// The parenthesised `FROM` element.
        table: Box<TableWithJoins>,
        /// Optional alias.
        alias: Option<TableAlias>,
    },
    /// A set-returning function, e.g. `generate_series(1, 3) g`.
    Function {
        /// The function name, as written.
        name: Name,
        /// The call arguments.
        args: Vec<Expr>,
        /// Optional alias.
        alias: Option<TableAlias>,
    },
}

/// A table alias with optional column aliases.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableAlias {
    /// The alias name.
    pub name: String,
    /// Renamed output columns, in order.
    pub columns: Vec<String>,
}

/// A single join.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Join {
    /// The joined relation.
    pub relation: TableFactor,
    /// The join type.
    pub op: JoinType,
    /// The join condition.
    pub constraint: JoinConstraint,
}

/// Join types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinType {
    /// `INNER JOIN`
    Inner,
    /// `LEFT [OUTER] JOIN`
    Left,
    /// `RIGHT [OUTER] JOIN`
    Right,
    /// `FULL [OUTER] JOIN`
    Full,
    /// `CROSS JOIN`
    Cross,
}

/// Join constraints.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum JoinConstraint {
    /// `ON <expr>`
    On(Expr),
    /// `USING (a, b)`
    Using(Vec<String>),
    /// `NATURAL JOIN`
    Natural,
    /// No constraint (only valid for `CROSS JOIN`).
    None,
}

/// An `ORDER BY` element.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrderByExpr {
    /// The sort key.
    pub expr: Expr,
    /// `true` for `ASC` (the default), `false` for `DESC`.
    pub asc: bool,
    /// Explicit `NULLS FIRST`/`NULLS LAST`; `None` means PostgreSQL's default
    /// (first for `DESC`, last for `ASC`).
    pub nulls_first: Option<bool>,
}

/// A column reference, possibly qualified by a table name or alias.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnRef {
    /// Table name/alias qualifier, if any.
    pub qualifier: Option<String>,
    /// Column name.
    pub name: String,
    /// `true` when the name was written in double quotes.
    ///
    /// Quoting is kept for two reasons: it decides whether the spelling is
    /// preserved (PostgreSQL folds only unquoted identifiers), and — when no
    /// column of that name is in scope — it lets an *unqualified* quoted name
    /// read as the string it spells, which is how a client written for MySQL
    /// means `WHERE type = "allowRegister"`. An unquoted name never does that,
    /// so a misspelled column stays an error instead of becoming a literal.
    pub quoted: bool,
}

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOp {
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Multiply,
    /// `/`
    Divide,
    /// `%`
    Modulo,
    /// `||`
    Concat,
    /// `=`
    Eq,
    /// `<>` / `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `IS DISTINCT FROM`
    IsDistinctFrom,
    /// `IS NOT DISTINCT FROM`
    IsNotDistinctFrom,
    /// `~`
    RegexMatch,
    /// `~*`
    RegexIMatch,
    /// `!~`
    RegexNotMatch,
    /// `!~*`
    RegexNotIMatch,
    /// `^`
    Pow,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `#`
    BitXor,
    /// `<<`
    ShiftLeft,
    /// `>>`
    ShiftRight,
}

/// A unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryOp {
    /// `-`
    Negate,
    /// `+`
    Plus,
    /// `NOT`
    Not,
    /// `~` (bitwise complement)
    BitNot,
}

/// A scalar expression.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    /// A literal constant.
    Literal(Value),
    /// A column reference.
    Column(ColumnRef),
    /// A binary operation.
    Binary {
        /// Left operand.
        left: Box<Expr>,
        /// The operator.
        op: BinaryOp,
        /// Right operand.
        right: Box<Expr>,
    },
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Expr>,
    },
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// The tested expression.
        expr: Box<Expr>,
        /// `true` for `IS NOT NULL`.
        negated: bool,
    },
    /// `expr IS [NOT] {TRUE|FALSE|UNKNOWN}`.
    IsBool {
        /// The tested expression.
        expr: Box<Expr>,
        /// `Some(true)`/`Some(false)` for `IS TRUE`/`IS FALSE`, `None` for `IS UNKNOWN`.
        value: Option<bool>,
        /// `true` for the `IS NOT ...` form.
        negated: bool,
    },
    /// A bind parameter, e.g. `$1` (1-based in the source, stored 0-based).
    Parameter(usize),
    /// `expr [NOT] IN (list)`.
    InList {
        /// The tested expression.
        expr: Box<Expr>,
        /// The candidate values.
        list: Vec<Expr>,
        /// `true` for `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] IN (subquery)`.
    InSubquery {
        /// The tested expression.
        expr: Box<Expr>,
        /// The subquery.
        subquery: Box<Query>,
        /// `true` for `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// The tested expression.
        expr: Box<Expr>,
        /// `true` for `NOT BETWEEN`.
        negated: bool,
        /// Inclusive lower bound.
        low: Box<Expr>,
        /// Inclusive upper bound.
        high: Box<Expr>,
    },
    /// `expr [NOT] LIKE/ILIKE pattern [ESCAPE c]`.
    Like {
        /// The tested expression.
        expr: Box<Expr>,
        /// The pattern.
        pattern: Box<Expr>,
        /// `true` for `NOT LIKE`.
        negated: bool,
        /// `true` for `ILIKE`.
        case_insensitive: bool,
        /// Optional `ESCAPE` character expression.
        escape: Option<Box<Expr>>,
    },
    /// An explicit cast: `expr::type` or `CAST(expr AS type)`.
    Cast {
        /// The source expression.
        expr: Box<Expr>,
        /// The target type.
        data_type: DataType,
        /// `true` for `TRY_CAST`, which yields `NULL` instead of an error.
        try_cast: bool,
    },
    /// A function call, including aggregates.
    Function(FunctionCall),
    /// A `CASE` expression.
    Case {
        /// Operand of the simple `CASE x WHEN ...` form.
        operand: Option<Box<Expr>>,
        /// `WHEN .. THEN ..` pairs.
        whens: Vec<(Expr, Expr)>,
        /// The `ELSE` result.
        else_result: Option<Box<Expr>>,
    },
    /// A scalar subquery, `(SELECT ...)`.
    Subquery(Box<Query>),
    /// `[NOT] EXISTS (SELECT ...)`.
    Exists {
        /// The subquery.
        subquery: Box<Query>,
        /// `true` for `NOT EXISTS`.
        negated: bool,
    },
    /// `expr op ANY/ALL (subquery|array)`.
    AnyAll {
        /// Left operand.
        left: Box<Expr>,
        /// The comparison operator.
        op: BinaryOp,
        /// Right-hand subquery or array expression.
        right: Box<Expr>,
        /// `true` for `ALL`, `false` for `ANY`/`SOME`.
        is_all: bool,
    },
    /// `ARRAY[...]`.
    Array(Vec<Expr>),
    /// A row/tuple constructor `(a, b)`.
    Row(Vec<Expr>),
    /// `expr[index]`: a one-based array element access.
    Subscript {
        /// The indexed expression.
        expr: Box<Expr>,
        /// The one-based index.
        index: Box<Expr>,
    },
    /// `*` in a function argument list.
    Wildcard,
    /// `alias.*`.
    QualifiedWildcard(String),
    /// A parenthesised expression, kept so that `EXPLAIN` can reproduce the input.
    Nested(Box<Expr>),
}

/// A function call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The function name, normalised to lower case.
    pub name: String,
    /// The arguments.
    pub args: Vec<Expr>,
    /// `COUNT(DISTINCT x)`.
    pub distinct: bool,
    /// `COUNT(*)`: the call had a `*` argument.
    pub star: bool,
    /// `agg(x) FILTER (WHERE cond)`.
    pub filter: Option<Box<Expr>>,
    /// An ordering written inside the argument list: `agg(x ORDER BY y)`.
    ///
    /// The planner accepts it for the aggregates the engine implements and
    /// drops it there: `count`, `sum`, `avg`, `min` and `max` do not observe
    /// the order of their inputs, so the clause cannot change the result.
    pub order_by: Vec<OrderByExpr>,
}

/// `INSERT` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    /// Target table.
    pub table: Name,
    /// Optional table alias (`INSERT INTO t AS x`).
    pub alias: Option<String>,
    /// Target columns; empty means "all columns in definition order".
    pub columns: Vec<String>,
    /// The row source.
    pub source: InsertSource,
    /// `RETURNING` items.
    pub returning: Vec<SelectItem>,
}

/// Where an `INSERT` gets its rows from.
#[derive(Clone, Debug, PartialEq)]
pub enum InsertSource {
    /// `VALUES (...), (...)`.
    Values(Vec<Vec<Expr>>),
    /// `INSERT ... SELECT`.
    Query(Box<Query>),
    /// `INSERT INTO t DEFAULT VALUES`.
    DefaultValues,
}

/// `UPDATE` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    /// Target table.
    pub table: Name,
    /// Optional table alias.
    pub alias: Option<String>,
    /// `SET col = expr` assignments.
    pub assignments: Vec<(String, Expr)>,
    /// `WHERE` predicate.
    pub selection: Option<Expr>,
    /// `RETURNING` items.
    pub returning: Vec<SelectItem>,
}

/// `DELETE` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    /// Target table.
    pub table: Name,
    /// Optional table alias.
    pub alias: Option<String>,
    /// `WHERE` predicate.
    pub selection: Option<Expr>,
    /// `RETURNING` items.
    pub returning: Vec<SelectItem>,
}

/// `CREATE TABLE` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateTable {
    /// Table name.
    pub name: Name,
    /// `IF NOT EXISTS`.
    pub if_not_exists: bool,
    /// `TEMPORARY`.
    pub temporary: bool,
    /// Column definitions, in declaration order.
    pub columns: Vec<ColumnDef>,
    /// Table-level constraints.
    pub constraints: Vec<TableConstraint>,
    /// `CREATE TABLE ... AS SELECT`.
    pub query: Option<Box<Query>>,
}

/// A column definition inside `CREATE TABLE`.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    /// Column name.
    pub name: String,
    /// Declared type.
    pub data_type: DataType,
    /// `false` when `NOT NULL` was declared.
    pub nullable: bool,
    /// `DEFAULT` expression.
    pub default: Option<Expr>,
    /// Column-level `PRIMARY KEY`.
    pub primary_key: bool,
    /// Column-level `UNIQUE`.
    pub unique: bool,
    /// Column-level `CHECK` constraints.
    pub checks: Vec<CheckConstraint>,
}

/// A `CHECK` constraint: the name declared with `CONSTRAINT`, if any, and the
/// condition every written row must satisfy.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckConstraint {
    /// `CONSTRAINT <name>`, when declared.
    pub name: Option<String>,
    /// The condition.
    pub expr: Expr,
}

/// A table-level constraint.
#[derive(Clone, Debug, PartialEq)]
pub enum TableConstraint {
    /// `PRIMARY KEY (a, b)`.
    PrimaryKey(Vec<String>),
    /// `UNIQUE (a, b)`.
    Unique(Vec<String>),
    /// `CHECK (expr)`.
    Check(CheckConstraint),
    /// `FOREIGN KEY (a) REFERENCES t (b)`.
    ForeignKey {
        /// `CONSTRAINT <name>`, when declared.
        name: Option<String>,
        /// Local columns.
        columns: Vec<String>,
        /// Referenced table.
        ref_table: Name,
        /// Referenced columns.
        ref_columns: Vec<String>,
    },
}

/// `ALTER TABLE` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct AlterTable {
    /// The table being altered.
    pub name: Name,
    /// The operations, in the order they were written.
    pub operations: Vec<AlterTableOperation>,
}

/// A single action of an `ALTER TABLE` statement.
///
/// Only the actions VelocitySQL can carry out are representable; anything else
/// (partitioning, storage parameters, replica identity, row level security, ...)
/// is rejected during lowering rather than accepted and ignored.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterTableOperation {
    /// `ADD [COLUMN] [IF NOT EXISTS] <column>`.
    AddColumn {
        /// `IF NOT EXISTS` was given.
        if_not_exists: bool,
        /// The column to append.
        column: ColumnDef,
    },
    /// `DROP [COLUMN] [IF EXISTS] <column>`.
    DropColumn {
        /// `IF EXISTS` was given.
        if_exists: bool,
        /// Column to drop.
        name: String,
    },
    /// `RENAME [COLUMN] <old> TO <new>`.
    RenameColumn {
        /// Current column name.
        name: String,
        /// New column name.
        new_name: String,
    },
    /// `RENAME TO <new name>`.
    RenameTable(Name),
    /// `ALTER [COLUMN] <name> [SET DATA] TYPE <type>`.
    AlterColumnType {
        /// Column name.
        name: String,
        /// The new type.
        data_type: DataType,
    },
    /// `ALTER [COLUMN] <name> SET DEFAULT <expr>`.
    SetDefault {
        /// Column name.
        name: String,
        /// The new default.
        default: Expr,
    },
    /// `ALTER [COLUMN] <name> DROP DEFAULT`.
    DropDefault {
        /// Column name.
        name: String,
    },
    /// `ALTER [COLUMN] <name> SET NOT NULL` / `DROP NOT NULL`.
    SetNotNull {
        /// Column name.
        name: String,
        /// `true` for `SET NOT NULL`, `false` for `DROP NOT NULL`.
        not_null: bool,
    },
    /// `ADD [CONSTRAINT <name>] <constraint>`.
    AddConstraint {
        /// Name given to the constraint, when the statement named it.
        name: Option<String>,
        /// The constraint to add.
        constraint: TableConstraint,
    },
    /// `DROP CONSTRAINT [IF EXISTS] <name>`.
    DropConstraint {
        /// `IF EXISTS` was given.
        if_exists: bool,
        /// Constraint to drop.
        name: String,
    },
    /// `OWNER TO <role>`.
    OwnerTo(String),
}

/// `CREATE INDEX` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateIndex {
    /// Index name; auto-generated from the table and columns when omitted.
    pub name: Option<String>,
    /// The indexed table.
    pub table: Name,
    /// The indexed columns.
    pub columns: Vec<IndexColumn>,
    /// `UNIQUE` index.
    pub unique: bool,
    /// `IF NOT EXISTS`.
    pub if_not_exists: bool,
    /// `USING btree|hash`.
    pub using: IndexMethod,
    /// Partial index predicate (not yet enforced, kept for round-tripping).
    pub predicate: Option<Expr>,
}

/// An indexed column.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexColumn {
    /// Column name.
    pub name: String,
    /// `true` for `ASC` (the default).
    pub asc: bool,
}

/// Index access methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexMethod {
    /// Ordered index, supports range scans.
    BTree,
    /// Equality-only index.
    Hash,
}

/// `CREATE VIEW` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateView {
    /// View name.
    pub name: Name,
    /// `OR REPLACE`.
    pub or_replace: bool,
    /// Optional column name list.
    pub columns: Vec<String>,
    /// The defining query.
    pub query: Box<Query>,
}

impl Expr {
    /// `true` when the expression is a literal constant.
    pub fn is_literal(&self) -> bool {
        matches!(self, Expr::Literal(_))
    }
}

/// An attribute in `CREATE ROLE` / `ALTER ROLE`.
///
/// Only the attributes VelocitySQL enforces are representable; anything else
/// (`INHERIT`, `BYPASSRLS`, `CONNECTION LIMIT`, `VALID UNTIL`, role membership)
/// is rejected during lowering rather than accepted and ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoleOption {
    /// `SUPERUSER` / `NOSUPERUSER`.
    Superuser(bool),
    /// `LOGIN` / `NOLOGIN`.
    Login(bool),
    /// `CREATEDB` / `NOCREATEDB`.
    CreateDb(bool),
    /// `CREATEROLE` / `NOCREATEROLE`.
    CreateRole(bool),
    /// `PASSWORD 'secret'` / `PASSWORD NULL`.
    Password(Option<String>),
}

impl Statement {
    /// How many bind parameters (`$1`, `$2`, ...) the statement uses.
    ///
    /// This is the count PostgreSQL reports in `ParameterDescription`, and
    /// every driver compares it against the arguments the caller supplied
    /// (`lib/pq` fails with *"got 3 parameters but the statement requires 0"*).
    /// The walk is over the parsed tree, so a `$` inside a string literal is
    /// text, not a placeholder.
    pub fn parameter_count(&self) -> usize {
        let mut highest = None;
        count_statement(self, &mut highest);
        // `$1` is stored as index 0, so the count is one past the highest index.
        highest.map_or(0, |index| index + 1)
    }
}

/// Remembers `index` as the highest parameter position seen so far.
fn note_parameter(highest: &mut Option<usize>, index: usize) {
    *highest = Some(match *highest {
        Some(current) => current.max(index),
        None => index,
    });
}

fn count_statement(statement: &Statement, highest: &mut Option<usize>) {
    match statement {
        Statement::Query(query) => count_query(query, highest),
        Statement::Insert(insert) => {
            match &insert.source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        row.iter().for_each(|expr| count_expr(expr, highest));
                    }
                }
                InsertSource::Query(query) => count_query(query, highest),
                InsertSource::DefaultValues => {}
            }
            insert
                .returning
                .iter()
                .for_each(|item| count_select_item(item, highest));
        }
        Statement::Update(update) => {
            update
                .assignments
                .iter()
                .for_each(|(_, expr)| count_expr(expr, highest));
            update
                .selection
                .iter()
                .for_each(|expr| count_expr(expr, highest));
            update
                .returning
                .iter()
                .for_each(|item| count_select_item(item, highest));
        }
        Statement::Delete(delete) => {
            delete
                .selection
                .iter()
                .for_each(|expr| count_expr(expr, highest));
            delete
                .returning
                .iter()
                .for_each(|item| count_select_item(item, highest));
        }
        Statement::CreateTable(create) => {
            for column in &create.columns {
                column
                    .default
                    .iter()
                    .for_each(|expr| count_expr(expr, highest));
                column
                    .checks
                    .iter()
                    .for_each(|check| count_expr(&check.expr, highest));
            }
            create
                .constraints
                .iter()
                .for_each(|constraint| count_constraint(constraint, highest));
            create.query.iter().for_each(|q| count_query(q, highest));
        }
        Statement::AlterTable(alter) => {
            for operation in &alter.operations {
                match operation {
                    AlterTableOperation::AddColumn { column, .. } => {
                        column
                            .default
                            .iter()
                            .for_each(|expr| count_expr(expr, highest));
                        column
                            .checks
                            .iter()
                            .for_each(|check| count_expr(&check.expr, highest));
                    }
                    AlterTableOperation::SetDefault { default, .. } => count_expr(default, highest),
                    AlterTableOperation::AddConstraint { constraint, .. } => {
                        count_constraint(constraint, highest)
                    }
                    _ => {}
                }
            }
        }
        Statement::CreateIndex(create) => {
            create
                .predicate
                .iter()
                .for_each(|expr| count_expr(expr, highest));
        }
        Statement::CreateView(view) => count_query(&view.query, highest),
        Statement::Explain { statement, .. } => count_statement(statement, highest),
        Statement::Set { value, .. } => {
            value.iter().for_each(|expr| count_expr(expr, highest));
        }
        // Nothing else can mention a placeholder: the remaining statements are
        // name-only DDL, session commands or transaction bookkeeping.
        _ => {}
    }
}

fn count_constraint(constraint: &TableConstraint, highest: &mut Option<usize>) {
    if let TableConstraint::Check(check) = constraint {
        count_expr(&check.expr, highest);
    }
}

fn count_select_item(item: &SelectItem, highest: &mut Option<usize>) {
    if let SelectItem::Expr { expr, .. } = item {
        count_expr(expr, highest);
    }
}

fn count_query(query: &Query, highest: &mut Option<usize>) {
    if let Some(with) = &query.with {
        with.ctes
            .iter()
            .for_each(|cte| count_query(&cte.query, highest));
    }
    count_set_expr(&query.body, highest);
    query
        .order_by
        .iter()
        .for_each(|item| count_expr(&item.expr, highest));
    query
        .limit
        .iter()
        .for_each(|expr| count_expr(expr, highest));
    query
        .offset
        .iter()
        .for_each(|expr| count_expr(expr, highest));
}

fn count_set_expr(set: &SetExpr, highest: &mut Option<usize>) {
    match set {
        SetExpr::Select(select) => {
            if let Some(Distinct::OnExprs(exprs)) = &select.distinct {
                exprs.iter().for_each(|expr| count_expr(expr, highest));
            }
            select
                .projection
                .iter()
                .for_each(|item| count_select_item(item, highest));
            select
                .from
                .iter()
                .for_each(|item| count_table_with_joins(item, highest));
            select
                .selection
                .iter()
                .for_each(|expr| count_expr(expr, highest));
            select
                .group_by
                .iter()
                .for_each(|expr| count_expr(expr, highest));
            select
                .having
                .iter()
                .for_each(|expr| count_expr(expr, highest));
        }
        SetExpr::Query(query) => count_query(query, highest),
        SetExpr::Values(rows) => {
            for row in rows {
                row.iter().for_each(|expr| count_expr(expr, highest));
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            count_set_expr(left, highest);
            count_set_expr(right, highest);
        }
    }
}

fn count_table_with_joins(item: &TableWithJoins, highest: &mut Option<usize>) {
    count_table_factor(&item.relation, highest);
    for join in &item.joins {
        count_table_factor(&join.relation, highest);
        if let JoinConstraint::On(expr) = &join.constraint {
            count_expr(expr, highest);
        }
    }
}

fn count_table_factor(factor: &TableFactor, highest: &mut Option<usize>) {
    match factor {
        TableFactor::Table { .. } => {}
        TableFactor::Derived { subquery, .. } => count_query(subquery, highest),
        TableFactor::NestedJoin { table, .. } => count_table_with_joins(table, highest),
        TableFactor::Function { args, .. } => {
            args.iter().for_each(|expr| count_expr(expr, highest));
        }
    }
}

fn count_expr(expr: &Expr, highest: &mut Option<usize>) {
    match expr {
        Expr::Parameter(index) => note_parameter(highest, *index),
        Expr::Literal(_) | Expr::Column(_) | Expr::Wildcard | Expr::QualifiedWildcard(_) => {}
        Expr::Nested(inner)
        | Expr::Unary { expr: inner, .. }
        | Expr::IsNull { expr: inner, .. }
        | Expr::IsBool { expr: inner, .. }
        | Expr::Cast { expr: inner, .. } => count_expr(inner, highest),
        Expr::Binary { left, right, .. } => {
            count_expr(left, highest);
            count_expr(right, highest);
        }
        Expr::InList { expr, list, .. } => {
            count_expr(expr, highest);
            list.iter().for_each(|item| count_expr(item, highest));
        }
        Expr::InSubquery { expr, subquery, .. } => {
            count_expr(expr, highest);
            count_query(subquery, highest);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            count_expr(expr, highest);
            count_expr(low, highest);
            count_expr(high, highest);
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            count_expr(expr, highest);
            count_expr(pattern, highest);
            escape.iter().for_each(|item| count_expr(item, highest));
        }
        Expr::Function(call) => {
            call.args.iter().for_each(|arg| count_expr(arg, highest));
            call.filter
                .iter()
                .for_each(|item| count_expr(item, highest));
            call.order_by
                .iter()
                .for_each(|item| count_expr(&item.expr, highest));
        }
        Expr::Case {
            operand,
            whens,
            else_result,
        } => {
            operand.iter().for_each(|item| count_expr(item, highest));
            for (condition, result) in whens {
                count_expr(condition, highest);
                count_expr(result, highest);
            }
            else_result
                .iter()
                .for_each(|item| count_expr(item, highest));
        }
        Expr::Subquery(query) => count_query(query, highest),
        Expr::Exists { subquery, .. } => count_query(subquery, highest),
        Expr::AnyAll { left, right, .. } => {
            count_expr(left, highest);
            count_expr(right, highest);
        }
        Expr::Array(items) | Expr::Row(items) => {
            items.iter().for_each(|item| count_expr(item, highest));
        }
        Expr::Subscript { expr, index } => {
            count_expr(expr, highest);
            count_expr(index, highest);
        }
    }
}
