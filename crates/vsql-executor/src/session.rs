//! The engine and the session that runs statements against it.
//!
//! # Layering
//!
//! `Engine` is the cluster: it owns one [`Database`] per `CREATE DATABASE`, and
//! each database owns its own catalog, rows and views. `Session` is
//! per-connection state (the database it attached to, transaction flags, `SET`
//! variables) plus the statement dispatch itself. Nothing in `Session` is shared
//! between connections, which keeps milestone M3's protocol layer a thin wrapper.
//!
//! # Statement execution
//!
//! * **Queries** go through the planner and then the volcano executor.
//! * **DML** binds its predicates and assignments against the target relation's
//!   schema, then walks the relation. Rows are touched one at a time so that a
//!   constraint violation aborts the statement *without* leaving a partially
//!   updated relation behind for the row in flight.
//! * **DDL** is applied to the catalog first and to storage second, so a failed
//!   catalog operation never leaves an orphaned relation in memory.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use vsql_catalog::{
    CatalogError, CheckDef, ColumnDef, ForeignKeyDef, IndexDef, IndexMethod, Oid, RoleDef,
    SchemaDef, TableDef,
};
use vsql_parser::ast::{
    AlterTable, AlterTableOperation, CreateIndex, CreateTable, CreateView, Delete, Expr, Insert,
    InsertSource, IsolationLevel, Query, SelectItem, Statement, TableConstraint, Update,
};
use vsql_storage::Table;
use vsql_types::{DataType, Row, Value};

use crate::catalog_scan;
use crate::database::{Database, Engine, DEFAULT_OWNER};
use crate::error::ExecError;
use crate::expr::{EvalEnv, ScalarExpr};
use crate::field::{Field, Schema};
use crate::plan::Plan;
use crate::planner::Planner;
use crate::{exec, explain, render};

/// One statement's result.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryResult {
    /// A result set.
    Rows {
        /// Column descriptions.
        schema: Schema,
        /// The rows.
        rows: Vec<Row>,
    },
    /// A statement that returned no rows, e.g. `INSERT`.
    Command {
        /// PostgreSQL's command tag, e.g. `INSERT 0 3`.
        tag: String,
    },
}

impl QueryResult {
    /// A row-returning result.
    pub fn rows(schema: Schema, rows: Vec<Row>) -> QueryResult {
        QueryResult::Rows { schema, rows }
    }

    /// A command result.
    pub fn command(tag: impl Into<String>) -> QueryResult {
        QueryResult::Command { tag: tag.into() }
    }

    /// The command tag the protocol layer reports.
    pub fn tag(&self) -> String {
        match self {
            QueryResult::Rows { rows, .. } => format!("SELECT {}", rows.len()),
            QueryResult::Command { tag } => tag.clone(),
        }
    }

    /// The result's columns, empty for commands.
    pub fn schema(&self) -> &[Field] {
        match self {
            QueryResult::Rows { schema, .. } => schema,
            QueryResult::Command { .. } => &[],
        }
    }

    /// The result's rows, empty for commands.
    pub fn data(&self) -> &[Row] {
        match self {
            QueryResult::Rows { rows, .. } => rows,
            QueryResult::Command { .. } => &[],
        }
    }
}

/// State of an open transaction.
#[derive(Clone, Copy, Debug)]
struct TransactionState {
    isolation: IsolationLevel,
    read_only: bool,
}

/// A client session.
#[derive(Debug)]
pub struct Session {
    engine: Arc<Engine>,
    /// Bind parameters for the current extended-query execution.
    parameters: Vec<Value>,
    /// The database this session is attached to.
    ///
    /// Pinned for the session's lifetime, exactly like a PostgreSQL backend:
    /// switching databases needs a new connection (`\c` in psql), which is what
    /// makes a reference from one database to another impossible rather than
    /// merely discouraged.
    database: Arc<Database>,
    /// The role this session acts as.
    ///
    /// The whole definition travels with the session, so a privilege check never
    /// re-reads the registry and a concurrent `ALTER ROLE` cannot change what an
    /// in-flight statement is allowed to do.
    role: RoleDef,
    variables: HashMap<String, String>,
    /// Parameters changed since the last statement, for the wire layer to report
    /// through `ParameterStatus` the way PostgreSQL does.
    parameter_updates: Vec<(String, String)>,
    transaction: Option<TransactionState>,
    statement_time: DateTime<Utc>,
}

/// The parameters PostgreSQL reports through `ParameterStatus`, as
/// `(wire name, GUC name)` pairs in the order PostgreSQL sends them at startup.
///
/// libpq and the drivers built on it (psql, JDBC, pgAdmin, ...) track these, and
/// the wire names are case-sensitive: `DateStyle` and `TimeZone` are reported in
/// mixed case, the rest in lower case.
const REPORTED_PARAMETERS: &[(&str, &str)] = &[
    ("application_name", "application_name"),
    ("client_encoding", "client_encoding"),
    ("DateStyle", "datestyle"),
    (
        "default_transaction_read_only",
        "default_transaction_read_only",
    ),
    ("in_hot_standby", "in_hot_standby"),
    ("integer_datetimes", "integer_datetimes"),
    ("IntervalStyle", "intervalstyle"),
    ("is_superuser", "is_superuser"),
    ("server_encoding", "server_encoding"),
    ("server_version", "server_version"),
    ("session_authorization", "session_authorization"),
    ("standard_conforming_strings", "standard_conforming_strings"),
    ("TimeZone", "timezone"),
];

/// Defaults for parameters clients ask about but this engine does not track.
///
/// `SHOW` reports them instead of failing: a tool that runs `SHOW datestyle`
/// while connecting should get PostgreSQL's default, not an error.
const PARAMETER_DEFAULTS: &[(&str, &str)] = &[
    ("application_name", ""),
    ("server_encoding", "UTF8"),
    ("bytea_output", "hex"),
    ("datestyle", "ISO, MDY"),
    ("default_transaction_isolation", "read committed"),
    ("default_transaction_read_only", "off"),
    ("extra_float_digits", "1"),
    ("idle_in_transaction_session_timeout", "0"),
    ("in_hot_standby", "off"),
    ("integer_datetimes", "on"),
    ("intervalstyle", "postgres"),
    ("lc_collate", "C"),
    ("lc_ctype", "C"),
    ("lock_timeout", "0"),
    ("max_identifier_length", "63"),
    ("row_security", "off"),
    ("statement_timeout", "0"),
    ("transaction_deferrable", "off"),
];

/// Parameters with a fixed value: `SET` refuses them, as PostgreSQL does.
const READ_ONLY_PARAMETERS: &[&str] = &[
    "in_hot_standby",
    "integer_datetimes",
    "is_superuser",
    "lc_collate",
    "lc_ctype",
    "max_identifier_length",
    "server_encoding",
    "server_version",
    "server_version_num",
    "session_authorization",
];

impl Session {
    /// Connects to the engine's default database.
    pub fn new(engine: Arc<Engine>) -> Self {
        Session::connect(engine, Engine::default_database_name(), DEFAULT_OWNER)
            .expect("`Engine::new` creates the default database")
    }

    /// Connects to a named database as the default role.
    pub fn with_database(engine: Arc<Engine>, database: &str) -> Result<Self, ExecError> {
        Session::connect(engine, database, DEFAULT_OWNER)
    }

    /// Connects to `database` as `user` without checking a password.
    ///
    /// Reserved for in-process callers (the embedded console, tests) and for
    /// `trust` authentication, where the transport is the security boundary. An
    /// unknown role still fails, so a typo cannot silently become a superuser.
    pub fn connect(engine: Arc<Engine>, database: &str, user: &str) -> Result<Self, ExecError> {
        let role = engine.identify(user)?;
        Session::open(engine, database, role)
    }

    /// Authenticates `user` with `password` and connects.
    ///
    /// This is what the wire protocol calls: the role must exist, be allowed to
    /// log in, and the password must match the stored SCRAM verifier. An unknown
    /// database yields `3D000` and a failed password `28P01`; both are forwarded
    /// to the client as `FATAL` before any session state is created.
    pub fn login(
        engine: Arc<Engine>,
        database: &str,
        user: &str,
        password: Option<&str>,
    ) -> Result<Self, ExecError> {
        let role = engine.authenticate(user, password)?;
        Session::open(engine, database, role)
    }

    /// Opens a session for a role whose credential has already been verified.
    ///
    /// The SCRAM exchange proves knowledge of the password without the server
    /// ever seeing it, so the protocol layer hands over the verified role rather
    /// than a password for a second check.
    pub fn with_role(
        engine: Arc<Engine>,
        database: &str,
        role: RoleDef,
    ) -> Result<Self, ExecError> {
        Session::open(engine, database, role)
    }

    /// Attaches an authenticated role to a database.
    fn open(engine: Arc<Engine>, database: &str, role: RoleDef) -> Result<Self, ExecError> {
        let database = engine.require_database(database)?;
        database.attach();
        let mut variables = HashMap::new();
        variables.insert("search_path".to_string(), "\"$user\", public".to_string());
        variables.insert("client_encoding".to_string(), "UTF8".to_string());
        variables.insert("timezone".to_string(), "UTC".to_string());
        variables.insert("standard_conforming_strings".to_string(), "on".to_string());
        Ok(Session {
            engine,
            database,
            role,
            variables,
            parameter_updates: Vec::new(),
            parameters: Vec::new(),
            transaction: None,
            statement_time: Utc::now(),
        })
    }

    /// The engine this session runs against.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// The database this session is attached to.
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    /// The attached database's name.
    pub fn database_name(&self) -> &str {
        self.database.name()
    }

    /// The role this session acts as.
    pub fn user(&self) -> &str {
        &self.role.name
    }

    /// `true` when the session's role bypasses privilege checks.
    pub fn is_superuser(&self) -> bool {
        self.role.superuser
    }

    /// Rejects an operation the connected role may not perform.
    ///
    /// `SUPERUSER` bypasses every check, exactly as in PostgreSQL, where the
    /// bootstrap role is all-powerful by construction.
    fn require_privilege(&self, allowed: bool, action: &str) -> Result<(), ExecError> {
        if allowed || self.role.superuser {
            return Ok(());
        }
        Err(ExecError::Catalog(CatalogError::PermissionDenied(
            action.to_string(),
        )))
    }

    /// Moves the session to another database, the way psql's `\c` does.
    pub fn switch_database(&mut self, name: &str) -> Result<(), ExecError> {
        let database = self.engine.require_database(name)?;
        if Arc::ptr_eq(&database, &self.database) {
            return Ok(());
        }
        self.database.detach();
        database.attach();
        self.database = database;
        // Transaction state belongs to the database that was open, so it does
        // not survive the switch.
        self.transaction = None;
        Ok(())
    }

    /// `true` while a transaction is open.
    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    /// Reports the result schema a statement will produce, without running it.
    ///
    /// The extended query protocol needs this for `Describe`: a client asks what
    /// a prepared statement returns *before* executing it, so that drivers can
    /// allocate the right buffers and type the columns.
    pub fn describe(&mut self, statement: &Statement) -> Result<Option<Schema>, ExecError> {
        match statement {
            Statement::Query(query) => {
                let plan = Planner::new(&self.database).plan_query(query)?;
                Ok(Some(plan.schema().to_vec()))
            }
            // `EXPLAIN` always answers with a single text column.
            Statement::Explain { .. } => {
                Ok(Some(vec![Field::unqualified("QUERY PLAN", DataType::Text)]))
            }
            // A `RETURNING` clause makes a write produce rows, and the client
            // asks for their description *before* it executes: PostgreSQL
            // answers with the `RETURNING` list, not with `NoData`. A driver
            // that only gets `NoData` here has no column names to hand the
            // rows it receives later.
            Statement::Insert(insert) if !insert.returning.is_empty() => {
                self.describe_returning(&insert.table, insert.alias.as_deref(), &insert.returning)
            }
            Statement::Update(update) if !update.returning.is_empty() => {
                self.describe_returning(&update.table, update.alias.as_deref(), &update.returning)
            }
            Statement::Delete(delete) if !delete.returning.is_empty() => {
                self.describe_returning(&delete.table, delete.alias.as_deref(), &delete.returning)
            }
            _ => Ok(None),
        }
    }

    /// The schema a `RETURNING` list will produce, for `Describe`.
    fn describe_returning(
        &self,
        table: &[String],
        alias: Option<&str>,
        returning: &[SelectItem],
    ) -> Result<Option<Schema>, ExecError> {
        let def = self.database.catalog.resolve_table(table)?;
        let table_schema = self.relation_schema(&def, alias);
        let mut planner = Planner::new(&self.database);
        let (_, schema) = self.bind_returning(&mut planner, returning, &table_schema)?;
        Ok(Some(schema))
    }

    /// Executes one or more SQL statements separated by semicolons.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, ExecError> {
        let statements = vsql_parser::parse(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in &statements {
            results.push(self.execute_statement(statement)?);
        }
        Ok(results)
    }

    /// Executes one or more statements with bind parameters supplied by the
    /// extended query protocol; `$1` resolves to `params[0]`.
    pub fn execute_with_params(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<QueryResult>, ExecError> {
        self.parameters = params.to_vec();
        let results = self.execute(sql);
        self.parameters.clear();
        results
    }

    /// Executes a single already-parsed statement.
    pub fn execute_statement(&mut self, statement: &Statement) -> Result<QueryResult, ExecError> {
        self.statement_time = Utc::now();
        let result = self.execute_statement_inner(statement);
        // The write is already visible in memory as this result goes back; ask
        // the persistence hook to snapshot in the background right after, so a
        // crash loses at most the statements still in flight.
        if result.is_ok() && is_mutating_statement(statement) {
            self.engine.notify_mutation();
        }
        result
    }

    fn execute_statement_inner(&mut self, statement: &Statement) -> Result<QueryResult, ExecError> {
        // PostgreSQL's `now()` is the transaction/statement start time; setting
        // it here makes every `now()` in one statement agree.
        self.statement_time = Utc::now();
        match statement {
            Statement::Query(query) => self.execute_query(query),
            Statement::Explain {
                analyze,
                statement,
                verbose: _,
            } => self.execute_explain(*analyze, statement),
            Statement::Insert(insert) => self.execute_insert(insert),
            Statement::Update(update) => self.execute_update(update),
            Statement::Delete(delete) => self.execute_delete(delete),
            Statement::CreateTable(create) => self.execute_create_table(create),
            Statement::CreateIndex(create) => self.execute_create_index(create),
            Statement::CreateView(view) => self.execute_create_view(view),
            Statement::AlterTable(alter) => self.execute_alter_table(alter),
            Statement::CreateDatabase {
                name,
                if_not_exists,
            } => {
                self.require_outside_transaction("CREATE DATABASE")?;
                self.require_privilege(self.role.create_db, "create database")?;
                // The registry lives on the engine, not on the attached
                // database: creating a database is a cluster-level operation.
                self.engine
                    .create_database(name, &self.role.name, *if_not_exists)?;
                Ok(QueryResult::command("CREATE DATABASE"))
            }
            Statement::CreateSchema {
                name,
                if_not_exists,
            } => {
                self.database.catalog.create_schema(name, *if_not_exists)?;
                Ok(QueryResult::command("CREATE SCHEMA"))
            }
            Statement::DropTable { if_exists, names } => {
                for name in names {
                    let (schema, relation) = self.split_relation(name)?;
                    let schema = self.require_schema(schema.as_deref(), "public")?;
                    if let Some(def) = self
                        .database
                        .catalog
                        .drop_table(schema.oid, &relation, *if_exists)?
                    {
                        self.database.storage.drop_table(def.oid);
                    }
                }
                Ok(QueryResult::command("DROP TABLE"))
            }
            Statement::CreateRole {
                name,
                if_not_exists,
                options,
            } => {
                self.require_privilege(self.role.create_role, "create role")?;
                self.engine.create_role(name, options, *if_not_exists)?;
                Ok(QueryResult::command("CREATE ROLE"))
            }
            Statement::AlterRole { name, options } => {
                self.require_privilege(self.role.create_role, "alter role")?;
                self.engine.alter_role(name, options)?;
                Ok(QueryResult::command("ALTER ROLE"))
            }
            Statement::DropRole { if_exists, names } => {
                // PostgreSQL reports the self-inflicted case before looking at
                // privileges, and the order is observable from the message.
                for name in names {
                    if name.eq_ignore_ascii_case(&self.role.name) {
                        return Err(ExecError::Catalog(CatalogError::CannotDropCurrentRole));
                    }
                }
                self.require_privilege(self.role.create_role, "drop role")?;
                for name in names {
                    self.engine
                        .drop_role(name, *if_exists, Some(self.role.name.as_str()))?;
                }
                Ok(QueryResult::command("DROP ROLE"))
            }
            Statement::DropDatabase { if_exists, names } => {
                self.require_outside_transaction("DROP DATABASE")?;
                for name in names {
                    self.engine
                        .drop_database(name, *if_exists, Some(self.database.name()))?;
                }
                Ok(QueryResult::command("DROP DATABASE"))
            }
            Statement::Truncate { names, if_exists } => {
                for name in names {
                    let def = match self.database.catalog.resolve_table(name) {
                        Ok(def) => def,
                        Err(_) if *if_exists => continue,
                        Err(error) => return Err(error.into()),
                    };
                    if let Some(table) = self.database.table(def.oid) {
                        table.truncate();
                    }
                }
                Ok(QueryResult::command("TRUNCATE TABLE"))
            }
            Statement::StartTransaction {
                isolation,
                read_only,
            } => {
                self.transaction = Some(TransactionState {
                    isolation: isolation.unwrap_or(IsolationLevel::ReadCommitted),
                    read_only: read_only.unwrap_or(false),
                });
                Ok(QueryResult::command("BEGIN"))
            }
            Statement::Commit => {
                self.transaction = None;
                Ok(QueryResult::command("COMMIT"))
            }
            Statement::Rollback { savepoint } => {
                if let Some(savepoint) = savepoint {
                    return Err(ExecError::not_supported(format!(
                        "ROLLBACK TO SAVEPOINT {savepoint} (arrives with MVCC in milestone M2)"
                    )));
                }
                self.transaction = None;
                Ok(QueryResult::command("ROLLBACK"))
            }
            Statement::Savepoint(name) => {
                if self.transaction.is_none() {
                    return Err(ExecError::other(
                        "SAVEPOINT can only be used in transaction blocks",
                    ));
                }
                Ok(QueryResult::command(format!("SAVEPOINT {name}")))
            }
            Statement::ReleaseSavepoint(name) => {
                Ok(QueryResult::command(format!("RELEASE {name}")))
            }
            Statement::Show(name) => self.execute_show(name),
            Statement::Set { name, value } => {
                let name = name.to_ascii_lowercase();
                if READ_ONLY_PARAMETERS.contains(&name.as_str()) {
                    return Err(ExecError::CannotChangeParameter(name));
                }
                if name == "client_encoding" {
                    // The engine speaks UTF-8 on the wire and cannot transcode,
                    // so anything else would be a setting that lies.
                    let requested = value
                        .as_ref()
                        .map(render_set_value)
                        .unwrap_or_else(|| "default".to_string());
                    let folded = requested.to_ascii_lowercase().replace('-', "");
                    if !matches!(folded.as_str(), "utf8" | "unicode" | "default") {
                        return Err(ExecError::not_supported(format!(
                            "client encoding \"{requested}\""
                        )));
                    }
                }
                match value {
                    // `SET x TO DEFAULT`: forget the override, report the default.
                    None => {
                        self.variables.remove(&name);
                        let default = self.session_guc(&name).unwrap_or_default();
                        self.record_parameter_update(&name, default);
                    }
                    Some(expr) => {
                        let rendered = render_set_value(expr);
                        // `SET x TO DEFAULT` reaches here as the identifier
                        // `DEFAULT`; it forgets the override instead of storing
                        // the word.
                        if rendered.eq_ignore_ascii_case("default") {
                            self.variables.remove(&name);
                            let default = self.session_guc(&name).unwrap_or_default();
                            self.record_parameter_update(&name, default);
                        } else {
                            self.variables.insert(name.clone(), rendered.clone());
                            self.record_parameter_update(&name, rendered);
                        }
                    }
                }
                Ok(QueryResult::command("SET"))
            }
        }
    }

    /// Rejects the statements PostgreSQL forbids inside a transaction block.
    fn require_outside_transaction(&self, statement: &str) -> Result<(), ExecError> {
        if self.transaction.is_some() {
            return Err(ExecError::ActiveSqlTransaction(statement.to_string()));
        }
        Ok(())
    }

    // ---------------------------------------------------------------- queries

    fn execute_query(&mut self, query: &Query) -> Result<QueryResult, ExecError> {
        let plan = Planner::new(&self.database).plan_query(query)?;
        let rows = self.run_plan(&plan)?;
        Ok(QueryResult::Rows {
            schema: plan.schema().to_vec(),
            rows,
        })
    }

    fn execute_explain(
        &mut self,
        analyze: bool,
        statement: &Statement,
    ) -> Result<QueryResult, ExecError> {
        let Statement::Query(query) = statement else {
            return Err(ExecError::not_supported("EXPLAIN for non-query statements"));
        };
        let plan = Planner::new(&self.database).plan_query(query)?;
        let actual = if analyze {
            Some(self.run_plan(&plan)?.len() as u64)
        } else {
            None
        };
        let lines = explain::explain(&plan, analyze, actual);
        Ok(QueryResult::Rows {
            schema: vec![Field::unqualified("QUERY PLAN", DataType::Text)],
            rows: lines
                .into_iter()
                .map(|line| vec![Value::Text(line)])
                .collect(),
        })
    }

    /// Evaluates a plan to completion with this session's statement timestamp.
    pub fn run_plan(&self, plan: &Plan) -> Result<Vec<Row>, ExecError> {
        exec::run_plan(plan, &self.environment())
    }

    /// The evaluation environment for this session.
    pub fn environment(&self) -> EvalEnv {
        EvalEnv {
            outer: Vec::new(),
            statement_time: Some(self.statement_time),
            database: Some(self.database.name().to_string()),
            user: Some(self.role.name.clone()),
            parameters: self.parameters.clone(),
            catalog: Some(std::sync::Arc::new(catalog_scan::snapshot(
                &self.database,
                &self.engine,
                self.settings_snapshot(),
                self.variables
                    .get("search_path")
                    .cloned()
                    .unwrap_or_else(|| "public".to_string()),
            ))),
        }
    }

    /// Every parameter as `(name, setting)`, the union `SHOW ALL` reports.
    fn settings_snapshot(&self) -> Vec<(String, String)> {
        let mut names: Vec<String> = self.variables.keys().cloned().collect();
        names.extend(PARAMETER_DEFAULTS.iter().map(|(name, _)| name.to_string()));
        names.extend(
            [
                "default_transaction_isolation",
                "is_superuser",
                "server_version",
                "server_version_num",
                "session_authorization",
                "transaction_isolation",
                "transaction_read_only",
            ]
            .map(String::from),
        );
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let value = self.session_guc(&name).unwrap_or_default();
                (name, value)
            })
            .collect()
    }

    // ------------------------------------------------------------------- DML

    fn execute_insert(&mut self, insert: &Insert) -> Result<QueryResult, ExecError> {
        let def = self.database.catalog.resolve_table(&insert.table)?;
        let table = self.require_table(&def)?;
        let target = self.insert_targets(&def, &insert.columns)?;
        let table_schema = self.relation_schema(&def, insert.alias.as_deref());

        let mut inserted: Vec<Row> = Vec::new();
        match &insert.source {
            InsertSource::Values(rows) => {
                let mut planner = Planner::new(&self.database);
                let mut provided = vec![false; def.column_count()];
                for &slot in &target {
                    provided[slot] = true;
                }
                for row in rows {
                    if row.len() != target.len() {
                        return Err(ExecError::other(format!(
                            "INSERT has {} expressions but {} target columns",
                            row.len(),
                            target.len()
                        )));
                    }
                    let mut values = vec![Value::Null; def.column_count()];
                    for (slot, expr) in row.iter().enumerate() {
                        let column = &def.columns[target[slot]];
                        let value = match expr {
                            // `INSERT ... VALUES (DEFAULT)`.
                            Expr::Column(reference)
                                if reference.name.eq_ignore_ascii_case("default") =>
                            {
                                self.default_for(column)?
                            }
                            other => {
                                let bound = planner.bind_expression(other)?;
                                // `eval` rather than `eval_constant`, so an
                                // explicit `nextval(...)` reaches the catalog.
                                let value = bound.eval(&Row::new(), &self.environment())?;
                                coerce(column, value)?
                            }
                        };
                        values[target[slot]] = value;
                    }
                    // Columns the statement did not provide take their defaults
                    // now — and only now, so an explicit value never advances a
                    // serial column's sequence.
                    for (ordinal, column) in def.columns.iter().enumerate() {
                        if !provided[ordinal] {
                            values[ordinal] = self.default_for(column)?;
                        }
                    }
                    inserted.push(values);
                }
            }
            InsertSource::Query(query) => {
                let plan = Planner::new(&self.database).plan_query(query)?;
                if plan.schema().len() != target.len() {
                    return Err(ExecError::other(format!(
                        "INSERT has {} target columns but the query returns {}",
                        target.len(),
                        plan.schema().len()
                    )));
                }
                let mut provided = vec![false; def.column_count()];
                for &slot in &target {
                    provided[slot] = true;
                }
                for row in self.run_plan(&plan)? {
                    let mut values = vec![Value::Null; def.column_count()];
                    for (slot, value) in row.into_iter().enumerate() {
                        let column = &def.columns[target[slot]];
                        values[target[slot]] = coerce(column, value)?;
                    }
                    for (ordinal, column) in def.columns.iter().enumerate() {
                        if !provided[ordinal] {
                            values[ordinal] = self.default_for(column)?;
                        }
                    }
                    inserted.push(values);
                }
            }
            InsertSource::DefaultValues => {
                inserted.push(self.default_row(&def)?);
            }
        }

        let checks = self.prepare_checks(&def)?;
        for row in &inserted {
            self.check_row(&def, &checks, row)?;
            table.insert(row.clone())?;
        }
        let count = inserted.len() as u64;

        if !insert.returning.is_empty() {
            let (exprs, schema) = self.bind_returning(
                &mut Planner::new(&self.database),
                &insert.returning,
                &table_schema,
            )?;
            let env = self.environment();
            let rows = self.project_rows(&exprs, &inserted, &env)?;
            return Ok(QueryResult::Rows { schema, rows });
        }
        Ok(QueryResult::command(format!("INSERT 0 {count}")))
    }

    fn execute_update(&mut self, update: &Update) -> Result<QueryResult, ExecError> {
        let def = self.database.catalog.resolve_table(&update.table)?;
        let table = self.require_table(&def)?;
        let table_schema = self.relation_schema(&def, update.alias.as_deref());
        let mut planner = Planner::new(&self.database);

        let mut assignments = Vec::with_capacity(update.assignments.len());
        for (name, expr) in &update.assignments {
            let index = def
                .column_index(name)
                .ok_or_else(|| ExecError::UndefinedColumn(name.clone()))?;
            let bound = planner.bind_expression_with_schema(expr, &table_schema)?;
            assignments.push((index, bound));
        }
        let predicate = match &update.selection {
            None => None,
            Some(selection) => Some(planner.bind_expression_with_schema(selection, &table_schema)?),
        };
        let (returning, returning_schema) =
            self.bind_returning(&mut planner, &update.returning, &table_schema)?;

        let env = self.environment();
        let checks = self.prepare_checks(&def)?;
        let mut updated = Vec::new();
        for rid in table.rids() {
            let Some(old) = table.row(rid) else {
                continue;
            };
            if let Some(predicate) = &predicate {
                if predicate.eval(&old, &env)?.as_bool() != Some(true) {
                    continue;
                }
            }
            let mut new_row = old.clone();
            for (index, expr) in &assignments {
                let value = expr.eval(&old, &env)?;
                new_row[*index] = coerce(&def.columns[*index], value)?;
            }
            self.check_row(&def, &checks, &new_row)?;
            if table.update(rid, new_row.clone())? {
                updated.push(new_row);
            }
        }

        if !returning.is_empty() {
            let rows = self.project_rows(&returning, &updated, &env)?;
            return Ok(QueryResult::Rows {
                schema: returning_schema,
                rows,
            });
        }
        Ok(QueryResult::command(format!("UPDATE {}", updated.len())))
    }

    fn execute_delete(&mut self, delete: &Delete) -> Result<QueryResult, ExecError> {
        let def = self.database.catalog.resolve_table(&delete.table)?;
        let table = self.require_table(&def)?;
        let table_schema = self.relation_schema(&def, delete.alias.as_deref());
        let mut planner = Planner::new(&self.database);
        let predicate = match &delete.selection {
            None => None,
            Some(selection) => Some(planner.bind_expression_with_schema(selection, &table_schema)?),
        };
        let (returning, returning_schema) =
            self.bind_returning(&mut planner, &delete.returning, &table_schema)?;

        let env = self.environment();
        let mut deleted = Vec::new();
        for rid in table.rids() {
            let Some(old) = table.row(rid) else {
                continue;
            };
            if let Some(predicate) = &predicate {
                if predicate.eval(&old, &env)?.as_bool() != Some(true) {
                    continue;
                }
            }
            if table.delete(rid)?.is_some() {
                deleted.push(old);
            }
        }

        if !returning.is_empty() {
            let rows = self.project_rows(&returning, &deleted, &env)?;
            return Ok(QueryResult::Rows {
                schema: returning_schema,
                rows,
            });
        }
        Ok(QueryResult::command(format!("DELETE {}", deleted.len())))
    }

    // ------------------------------------------------------------------- DDL

    /// Executes `ALTER TABLE`.
    ///
    /// Operations run in the order they were written, and each one sees what the
    /// previous ones did - which is why a rename hands the new name back instead
    /// of the statement carrying on with the old one.
    fn execute_alter_table(&mut self, alter: &AlterTable) -> Result<QueryResult, ExecError> {
        let mut name = alter.name.clone();
        for operation in &alter.operations {
            name = self.execute_alter_table_operation(&name, operation)?;
        }
        Ok(QueryResult::command("ALTER TABLE"))
    }

    fn execute_alter_table_operation(
        &mut self,
        name: &[String],
        operation: &AlterTableOperation,
    ) -> Result<Vec<String>, ExecError> {
        match operation {
            AlterTableOperation::RenameTable(new_name) => self.alter_rename_table(name, new_name),
            AlterTableOperation::AddColumn {
                if_not_exists,
                column,
            } => {
                self.alter_add_column(name, column, *if_not_exists)?;
                Ok(name.to_vec())
            }
            AlterTableOperation::DropColumn {
                if_exists,
                name: column,
            } => {
                self.alter_drop_column(name, column, *if_exists)?;
                Ok(name.to_vec())
            }
            AlterTableOperation::RenameColumn {
                name: column,
                new_name,
            } => {
                self.alter_rename_column(name, column, new_name)?;
                Ok(name.to_vec())
            }
            AlterTableOperation::AlterColumnType {
                name: column,
                data_type,
            } => {
                self.alter_column_type(name, column, data_type)?;
                Ok(name.to_vec())
            }
            AlterTableOperation::SetDefault {
                name: column,
                default,
            } => {
                self.alter_set_default(name, column, Some(default))?;
                Ok(name.to_vec())
            }
            AlterTableOperation::DropDefault { name: column } => {
                self.alter_set_default(name, column, None)?;
                Ok(name.to_vec())
            }
            AlterTableOperation::SetNotNull {
                name: column,
                not_null,
            } => {
                self.alter_set_not_null(name, column, *not_null)?;
                Ok(name.to_vec())
            }
            AlterTableOperation::AddConstraint {
                name: constraint_name,
                constraint,
            } => {
                self.alter_add_constraint(name, constraint_name.as_deref(), constraint)?;
                Ok(name.to_vec())
            }
            AlterTableOperation::DropConstraint {
                if_exists,
                name: constraint,
            } => {
                self.alter_drop_constraint(name, constraint, *if_exists)?;
                Ok(name.to_vec())
            }
            AlterTableOperation::OwnerTo(role) => {
                self.alter_owner_to(name, role)?;
                Ok(name.to_vec())
            }
        }
    }

    /// The definition and the storage table an `ALTER TABLE` operation works on.
    fn alter_target(&self, name: &[String]) -> Result<(Arc<TableDef>, Arc<Table>), ExecError> {
        let (_, relation) = self.split_relation(name)?;
        if self.database.view(name).is_some() {
            // PostgreSQL says the same thing for a view and for a materialised
            // view: only a table can be altered, and a view has no storage of its
            // own to change.
            return Err(ExecError::Catalog(CatalogError::Other(format!(
                "\"{relation}\" is not a table"
            ))));
        }
        let def = self.database.catalog.resolve_table(name)?;
        let table = self.require_table(&def)?;
        Ok((def, table))
    }

    /// Installs `layout` on the table, leaving the stored rows alone.
    ///
    /// For a change that does not move data - a rename, a new default, a
    /// nullability flag. A change to the columns' *shape* goes through
    /// [`Session::reshape`], which rewrites the rows to match.
    fn install_layout(
        &self,
        old: &Arc<TableDef>,
        layout: TableLayout,
    ) -> Result<Arc<TableDef>, ExecError> {
        let def = self.database.catalog.replace_table(layout.into_def(old))?;
        self.require_table(&def)?.set_def(def.clone());
        Ok(def)
    }

    /// Installs `layout` and rewrites every row with `f` so the stored data
    /// matches it.
    ///
    /// The rows are converted first: [`Table::restructure`] either rewrites all
    /// of them or none, so a conversion that fails half way through - a value the
    /// new type cannot hold, say - leaves the catalog naming the layout the rows
    /// are still in.
    fn reshape<E>(
        &self,
        old: &Arc<TableDef>,
        table: &Arc<Table>,
        layout: TableLayout,
        f: impl FnMut(&Row) -> Result<Row, E>,
    ) -> Result<Arc<TableDef>, ExecError>
    where
        E: Into<ExecError>,
    {
        let def = layout.into_def(old);
        table.restructure(def.clone(), f).map_err(Into::into)?;
        Ok(self.database.catalog.replace_table(def)?)
    }

    /// `ALTER TABLE ... ADD COLUMN`.
    ///
    /// The column is appended, so the ordinals of the columns that are already
    /// there - and therefore the meaning of every stored row and index key - do
    /// not move.
    fn alter_add_column(
        &mut self,
        name: &[String],
        column: &vsql_parser::ast::ColumnDef,
        if_not_exists: bool,
    ) -> Result<(), ExecError> {
        if !column.checks.is_empty() {
            return Err(ExecError::not_supported("CHECK constraints"));
        }
        let (def, table) = self.alter_target(name)?;
        let mut layout = TableLayout::of(&def);
        if layout.has_column(&column.name) {
            if if_not_exists {
                return Ok(());
            }
            return Err(ExecError::Catalog(CatalogError::DuplicateColumn(
                column.name.clone(),
            )));
        }
        let mut added = ColumnDef::new(
            column.name.clone(),
            column.data_type.clone(),
            layout.columns.len(),
        );
        added.nullable = column.nullable;
        added.default = column.default.as_ref().map(render::ast_expr);
        if let Some(sequence) = default_sequence(&column.default) {
            self.ensure_sequence(def.schema_oid, &sequence)?;
        }
        if column.primary_key && !layout.primary_key.is_empty() {
            return Err(ExecError::Catalog(CatalogError::Other(format!(
                "multiple primary keys for table \"{}\" are not allowed",
                def.name
            ))));
        }
        // The rows that are already there have no value for the new column, so
        // they take the default - which is exactly why a `NOT NULL` column
        // without one cannot be added to a table that is not empty.
        if !added.nullable && added.default.is_none() && !table.is_empty() {
            return Err(ExecError::Storage(
                vsql_storage::StorageError::NotNullViolation {
                    table: def.name.clone(),
                    column: column.name.clone(),
                },
            ));
        }
        let fill = self.default_for(&added)?;
        let ordinal = added.ordinal;
        if column.primary_key {
            layout.primary_key = vec![ordinal];
        }
        layout.columns.push(added);

        let def = self.reshape(&def, &table, layout, |row| {
            let mut extended = row.clone();
            extended.push(fill.clone());
            Ok::<Row, ExecError>(extended)
        })?;
        if column.unique || column.primary_key {
            let table = self.require_table(&def)?;
            let index_name = if column.primary_key {
                format!("{}_pkey", def.name)
            } else {
                format!("{}_{}_key", def.name, column.name)
            };
            self.attach_index(
                &table,
                &def,
                index_name,
                vec![ordinal],
                true,
                column.primary_key,
                IndexMethod::BTree,
            )?;
        }
        Ok(())
    }

    /// `ALTER TABLE ... DROP COLUMN`.
    ///
    /// The column's value stays in every row: PostgreSQL keeps the physical slot,
    /// which is what makes a later `ADD COLUMN` take a fresh ordinal instead of
    /// turning a stored value into another column's data.
    fn alter_drop_column(
        &mut self,
        name: &[String],
        column: &str,
        if_exists: bool,
    ) -> Result<(), ExecError> {
        let (def, table) = self.alter_target(name)?;
        let mut layout = TableLayout::of(&def);
        let Some(ordinal) = layout.column_index(column) else {
            if if_exists {
                return Ok(());
            }
            return Err(ExecError::UndefinedColumn(column.to_string()));
        };
        // PostgreSQL drops the constraints the column took part in, so the
        // indexes that reference it go with it - the primary key included.
        let dependent: Vec<Arc<IndexDef>> = self
            .database
            .catalog
            .table_indexes(def.oid)
            .into_iter()
            .filter(|index| index.columns.contains(&ordinal))
            .collect();
        layout.columns[ordinal].dropped = true;
        if layout.primary_key.contains(&ordinal) {
            layout.primary_key.clear();
        }
        // The definition is installed first: it is what carries the index list,
        // so dropping the indexes afterwards removes them from the layout the
        // catalog is holding.
        self.install_layout(&def, layout)?;
        for index in dependent {
            self.database.catalog.drop_index(index.oid);
            table.detach_index(index.oid);
        }
        Ok(())
    }

    fn alter_rename_column(
        &mut self,
        name: &[String],
        column: &str,
        new_name: &str,
    ) -> Result<(), ExecError> {
        let (def, _) = self.alter_target(name)?;
        let mut layout = TableLayout::of(&def);
        let ordinal = layout.column(column)?;
        if layout.has_column(new_name) {
            return Err(ExecError::Catalog(CatalogError::DuplicateColumn(
                new_name.to_string(),
            )));
        }
        layout.columns[ordinal].name = new_name.to_string();
        self.install_layout(&def, layout)?;
        Ok(())
    }

    fn alter_rename_table(
        &mut self,
        name: &[String],
        new_name: &[String],
    ) -> Result<Vec<String>, ExecError> {
        let (def, _) = self.alter_target(name)?;
        let (_, relation) = self.split_relation(new_name)?;
        // `RENAME TO` takes a bare name: the table stays in the schema it is in,
        // and taking a qualifier here would be a silent `SET SCHEMA`.
        let mut layout = TableLayout::of(&def);
        layout.name = relation.clone();
        self.install_layout(&def, layout)?;
        let mut renamed = name.to_vec();
        if let Some(last) = renamed.last_mut() {
            *last = relation;
        }
        Ok(renamed)
    }

    /// `ALTER TABLE ... ALTER COLUMN ... TYPE`.
    ///
    /// Stored values are converted with the same cast an `INSERT` applies, and a
    /// value the new type cannot represent aborts the statement with the table
    /// left exactly as it was.
    fn alter_column_type(
        &mut self,
        name: &[String],
        column: &str,
        data_type: &DataType,
    ) -> Result<(), ExecError> {
        let (def, table) = self.alter_target(name)?;
        let mut layout = TableLayout::of(&def);
        let ordinal = layout.column(column)?;
        layout.columns[ordinal].data_type = data_type.clone();
        let target = layout.columns[ordinal].clone();
        self.reshape(&def, &table, layout, move |row| {
            let mut converted = row.clone();
            converted[ordinal] = coerce(&target, converted[ordinal].clone())?;
            Ok::<Row, ExecError>(converted)
        })?;
        Ok(())
    }

    /// `ALTER TABLE ... ALTER COLUMN ... SET DEFAULT` / `DROP DEFAULT`.
    fn alter_set_default(
        &mut self,
        name: &[String],
        column: &str,
        default: Option<&Expr>,
    ) -> Result<(), ExecError> {
        let (def, _) = self.alter_target(name)?;
        let mut layout = TableLayout::of(&def);
        let ordinal = layout.column(column)?;
        layout.columns[ordinal].default = default.map(render::ast_expr);
        self.install_layout(&def, layout)?;
        Ok(())
    }

    /// `ALTER TABLE ... ALTER COLUMN ... SET/DROP NOT NULL`.
    fn alter_set_not_null(
        &mut self,
        name: &[String],
        column: &str,
        not_null: bool,
    ) -> Result<(), ExecError> {
        let (def, table) = self.alter_target(name)?;
        let mut layout = TableLayout::of(&def);
        let ordinal = layout.column(column)?;
        if not_null {
            // Every stored row has to satisfy the constraint before it is added.
            let mut offending = false;
            table.for_each(|_, row| {
                if row.get(ordinal).is_none_or(Value::is_null) {
                    offending = true;
                    return false;
                }
                true
            });
            if offending {
                return Err(ExecError::Storage(
                    vsql_storage::StorageError::NotNullViolation {
                        table: def.name.clone(),
                        column: layout.columns[ordinal].name.clone(),
                    },
                ));
            }
        }
        layout.columns[ordinal].nullable = !not_null;
        self.install_layout(&def, layout)?;
        Ok(())
    }

    /// `ALTER TABLE ... ADD CONSTRAINT`.
    ///
    /// A primary key and a unique constraint are backed by a unique index, a
    /// `CHECK` is validated against the existing rows and then evaluated per
    /// written row, and a `FOREIGN KEY` is recorded as metadata (`pg_constraint`
    /// reports it); the reference itself is not probed on later writes.
    fn alter_add_constraint(
        &mut self,
        name: &[String],
        constraint_name: Option<&str>,
        constraint: &TableConstraint,
    ) -> Result<(), ExecError> {
        let (def, table) = self.alter_target(name)?;
        match constraint {
            TableConstraint::Check(check) => {
                let base = check
                    .name
                    .clone()
                    .or_else(|| constraint_name.map(str::to_string))
                    .unwrap_or_else(|| format!("{}_check", def.name));
                let mut names: HashSet<String> =
                    def.checks.iter().map(|c| c.name.clone()).collect();
                let name = unique_check_name(&base, &mut names);
                let mut all = def.checks.clone();
                all.push(CheckDef {
                    oid: self.database.catalog.next_oid(),
                    name,
                    expr: render::ast_expr(&check.expr),
                });
                // PostgreSQL validates the new constraint against the existing
                // rows before it installs it; do the same with the full set.
                let schema = self.relation_schema(&def, None);
                let mut prepared = Vec::with_capacity(all.len());
                for check in &all {
                    let parsed = vsql_parser::parse_expression(&check.expr)?;
                    let bound = Planner::new(&self.database)
                        .bind_expression_with_schema(&parsed, &schema)?;
                    prepared.push((check.name.clone(), bound));
                }
                for rid in table.rids() {
                    let Some(row) = table.row(rid) else {
                        continue;
                    };
                    self.check_row(&def, &prepared, &row)?;
                }
                let def = self.database.catalog.set_checks(def.oid, all)?;
                table.set_def(def);
                return Ok(());
            }
            TableConstraint::ForeignKey {
                name: declared_name,
                columns,
                ref_table,
                ref_columns,
            } => {
                let key = self.resolve_foreign_key(
                    &def.name,
                    &def.columns,
                    columns,
                    ref_table,
                    ref_columns,
                    declared_name.as_deref().or(constraint_name),
                )?;
                let def = self.database.catalog.attach_foreign_key(def.oid, key)?;
                table.set_def(def);
                return Ok(());
            }
            _ => {}
        }
        let (columns, primary) = match constraint {
            TableConstraint::PrimaryKey(names) => {
                (self.resolve_columns(&def.columns, names)?, true)
            }
            TableConstraint::Unique(names) => (self.resolve_columns(&def.columns, names)?, false),
            TableConstraint::Check(_) | TableConstraint::ForeignKey { .. } => unreachable!(),
        };
        // PostgreSQL reports a second primary key before it ever worries about
        // what the constraint's index would be called.
        let mut layout = TableLayout::of(&def);
        if primary {
            if !layout.primary_key.is_empty() {
                return Err(ExecError::Catalog(CatalogError::Other(format!(
                    "multiple primary keys for table \"{}\" are not allowed",
                    def.name
                ))));
            }
            layout.primary_key = columns.clone();
        }
        let index_name = match constraint_name {
            Some(name) => name.to_string(),
            None if primary => format!("{}_pkey", def.name),
            None => format!(
                "{}_{}_key",
                def.name,
                self.column_names(&def, &columns).join("_")
            ),
        };
        if self
            .database
            .catalog
            .index_named(def.schema_oid, &index_name)
            .is_some()
        {
            return Err(ExecError::Catalog(CatalogError::IndexExists(index_name)));
        }
        let def = self.install_layout(&def, layout)?;
        let table = self.require_table(&def)?;
        // Unique either way: that is what the constraint means, and the backfill
        // check inside `attach_index` is what reports data that violates it.
        self.attach_index(
            &table,
            &def,
            index_name,
            columns,
            true,
            primary,
            IndexMethod::BTree,
        )?;
        Ok(())
    }

    /// `ALTER TABLE ... DROP CONSTRAINT`.
    fn alter_drop_constraint(
        &mut self,
        name: &[String],
        constraint: &str,
        if_exists: bool,
    ) -> Result<(), ExecError> {
        let (def, table) = self.alter_target(name)?;
        let found = self
            .database
            .catalog
            .table_indexes(def.oid)
            .into_iter()
            .find(|index| index.name.eq_ignore_ascii_case(constraint));
        let Some(index) = found else {
            if if_exists {
                return Ok(());
            }
            return Err(ExecError::Catalog(CatalogError::UndefinedConstraint {
                table: def.name.clone(),
                constraint: constraint.to_string(),
            }));
        };
        if index.primary && !def.primary_key.is_empty() {
            let mut layout = TableLayout::of(&def);
            layout.primary_key.clear();
            self.install_layout(&def, layout)?;
        }
        self.database.catalog.drop_index(index.oid);
        table.detach_index(index.oid);
        Ok(())
    }

    /// `ALTER TABLE ... OWNER TO`.
    fn alter_owner_to(&mut self, name: &[String], role: &str) -> Result<(), ExecError> {
        let (def, _) = self.alter_target(name)?;
        // PostgreSQL refuses to hand a relation to a role that is not there.
        let owner = self
            .engine
            .role(role)
            .ok_or_else(|| ExecError::Catalog(CatalogError::UndefinedRole(role.to_string())))?;
        let mut layout = TableLayout::of(&def);
        layout.owner = Some(owner.name);
        self.install_layout(&def, layout)?;
        Ok(())
    }

    fn execute_create_table(&mut self, create: &CreateTable) -> Result<QueryResult, ExecError> {
        let (schema_name, relation) = self.split_relation(&create.name)?;
        let schema = self.require_schema(schema_name.as_deref(), "public")?;

        let mut columns = Vec::with_capacity(create.columns.len());
        let mut primary_key: Vec<usize> = Vec::new();
        let mut unique_sets: Vec<Vec<usize>> = Vec::new();
        let mut checks: Vec<(String, String)> = Vec::new();
        let mut used_check_names: HashSet<String> = HashSet::new();
        // A foreign key as the parser reports it: its declared name if any, its
        // columns, the table it references and that table's columns.
        type ForeignKeySpec = (Option<String>, Vec<String>, Vec<String>, Vec<String>);
        let mut foreign_keys: Vec<ForeignKeySpec> = Vec::new();

        for column in &create.columns {
            let mut def =
                ColumnDef::new(column.name.clone(), column.data_type.clone(), columns.len());
            def.nullable = column.nullable;
            def.default = column.default.as_ref().map(render::ast_expr);
            columns.push(def);
            let index = columns.len() - 1;
            if column.primary_key {
                primary_key.push(index);
            }
            if column.unique {
                unique_sets.push(vec![index]);
            }
            for check in &column.checks {
                let base = check
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{}_{}_check", relation, column.name));
                let name = unique_check_name(&base, &mut used_check_names);
                checks.push((name, render::ast_expr(&check.expr)));
            }
        }
        for constraint in &create.constraints {
            match constraint {
                TableConstraint::PrimaryKey(names) => {
                    primary_key = self.resolve_columns(&columns, names)?;
                }
                TableConstraint::Unique(names) => {
                    unique_sets.push(self.resolve_columns(&columns, names)?);
                }
                TableConstraint::Check(check) => {
                    let base = check
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("{}_check", relation));
                    let name = unique_check_name(&base, &mut used_check_names);
                    checks.push((name, render::ast_expr(&check.expr)));
                }
                TableConstraint::ForeignKey {
                    name: declared_name,
                    columns: fk_columns,
                    ref_table,
                    ref_columns,
                } => {
                    // Resolved after the table exists: a self-reference
                    // (`uid INT REFERENCES t (id)` on table `t`) is legal in
                    // PostgreSQL because the constraint is added once the
                    // table is.
                    foreign_keys.push((
                        declared_name.clone(),
                        fk_columns.clone(),
                        ref_table.clone(),
                        ref_columns.clone(),
                    ));
                }
            }
        }

        let mut def = self.database.catalog.create_table(
            schema.oid,
            &relation,
            columns,
            primary_key.clone(),
            create.if_not_exists,
            false,
        )?;
        // Serial columns carry a `nextval` default; the sequence they advance
        // is created here. `IF NOT EXISTS` keeps a re-run idempotent.
        for column in &create.columns {
            if let Some(sequence) = default_sequence(&column.default) {
                self.ensure_sequence(schema.oid, &sequence)?;
            }
        }
        if !checks.is_empty() {
            let defs = checks
                .into_iter()
                .map(|(name, expr)| CheckDef {
                    oid: self.database.catalog.next_oid(),
                    name,
                    expr,
                })
                .collect();
            def = self.database.catalog.set_checks(def.oid, defs)?;
        }
        for (name, columns, ref_table, ref_columns) in foreign_keys {
            let key = self.resolve_foreign_key(
                &relation,
                &def.columns,
                &columns,
                &ref_table,
                &ref_columns,
                name.as_deref(),
            )?;
            def = self.database.catalog.attach_foreign_key(def.oid, key)?;
        }
        let table = self.database.mount(def.clone());

        if !primary_key.is_empty() {
            let name = format!("{}_pkey", def.name);
            self.attach_index(
                &table,
                &def,
                name,
                primary_key.clone(),
                true,
                true,
                IndexMethod::BTree,
            )?;
        }
        for columns in unique_sets {
            let name = format!(
                "{}_{}_key",
                def.name,
                self.column_names(&def, &columns).join("_")
            );
            self.attach_index(&table, &def, name, columns, true, false, IndexMethod::BTree)?;
        }

        if let Some(query) = &create.query {
            self.insert_from_query(&def, &table, query)?;
        }
        Ok(QueryResult::command("CREATE TABLE"))
    }

    fn execute_create_index(&mut self, create: &CreateIndex) -> Result<QueryResult, ExecError> {
        let def = self.database.catalog.resolve_table(&create.table)?;
        let table = self.require_table(&def)?;
        let mut columns = Vec::with_capacity(create.columns.len());
        for column in &create.columns {
            let index = def
                .column_index(&column.name)
                .ok_or_else(|| ExecError::UndefinedColumn(column.name.clone()))?;
            columns.push(index);
        }
        let name = match &create.name {
            Some(name) => name.clone(),
            None => format!(
                "{}_{}_idx",
                def.name,
                self.column_names(&def, &columns).join("_")
            ),
        };
        let schema = self
            .database
            .catalog
            .resolve_schema("public")
            .unwrap_or(SchemaDef {
                oid: def.schema_oid,
                name: "public".to_string(),
                is_system: false,
            });
        if let Some(existing) = self.database.catalog.index_named(schema.oid, &name) {
            if create.if_not_exists {
                let _ = existing;
                return Ok(QueryResult::command("CREATE INDEX"));
            }
            return Err(ExecError::Catalog(CatalogError::IndexExists(name)));
        }
        let method = match create.using {
            vsql_parser::ast::IndexMethod::BTree => IndexMethod::BTree,
            vsql_parser::ast::IndexMethod::Hash => IndexMethod::Hash,
        };
        self.attach_index(&table, &def, name, columns, create.unique, false, method)?;
        Ok(QueryResult::command("CREATE INDEX"))
    }

    fn execute_create_view(&mut self, view: &CreateView) -> Result<QueryResult, ExecError> {
        // Plan the body first: a view that cannot be planned must not be created.
        Planner::new(&self.database).plan_query(&view.query)?;
        let (schema_name, relation) = self.split_relation(&view.name)?;
        let schema = self.require_schema(schema_name.as_deref(), "public")?;
        self.database.create_view(
            schema.oid,
            &relation,
            (*view.query).clone(),
            view.or_replace,
        )?;
        Ok(QueryResult::command("CREATE VIEW"))
    }

    fn execute_show(&mut self, name: &str) -> Result<QueryResult, ExecError> {
        if name.eq_ignore_ascii_case("all") {
            return Ok(self.show_all());
        }
        let value = self.session_guc(name).ok_or_else(|| {
            // PostgreSQL refuses unknown names too, so a typo stays a typo.
            ExecError::other(format!("unrecognized configuration parameter \"{name}\""))
        })?;
        Ok(QueryResult::Rows {
            schema: vec![Field::unqualified(name, DataType::Text)],
            rows: vec![vec![Value::Text(value)]],
        })
    }

    /// The value of a configuration parameter, or `None` when even PostgreSQL
    /// does not know the name.
    ///
    /// Names are matched case-insensitively: `SHOW DATESTYLE` and `SHOW
    /// datestyle` are the same parameter, exactly as in PostgreSQL.
    fn session_guc(&self, name: &str) -> Option<String> {
        let folded = name.to_ascii_lowercase();
        let value = match folded.as_str() {
            "server_version" => format!("15.0 (VelocitySQL {})", env!("CARGO_PKG_VERSION")),
            "server_version_num" => "150000".to_string(),
            "transaction_isolation" => self
                .transaction
                .map(|state| isolation_name(state.isolation).to_string())
                .unwrap_or_else(|| "read committed".to_string()),
            "default_transaction_isolation" => self
                .variables
                .get("default_transaction_isolation")
                .cloned()
                .unwrap_or_else(|| "read committed".to_string()),
            // The *current* transaction's setting; the default for future ones
            // is a plain GUC and lives in `variables`.
            "transaction_read_only" => self
                .transaction
                .map(|state| if state.read_only { "on" } else { "off" })
                .unwrap_or("off")
                .to_string(),
            "session_authorization" => self.role.name.clone(),
            "is_superuser" => if self.role.superuser { "on" } else { "off" }.to_string(),
            other => {
                // An explicit `SET` wins; otherwise the documented default.
                return self.variables.get(other).cloned().or_else(|| {
                    PARAMETER_DEFAULTS
                        .iter()
                        .find(|(known, _)| *known == other)
                        .map(|(_, default)| default.to_string())
                });
            }
        };
        Some(value)
    }

    /// The parameters PostgreSQL reports through `ParameterStatus`, in order.
    pub fn reported_parameters(&self) -> Vec<(&'static str, String)> {
        REPORTED_PARAMETERS
            .iter()
            .map(|(wire, guc)| {
                (
                    *wire,
                    self.session_guc(guc).unwrap_or_else(|| "off".to_string()),
                )
            })
            .collect()
    }

    /// Drains the parameters changed since the last statement, for the wire
    /// layer to report through `ParameterStatus`.
    pub fn take_parameter_updates(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.parameter_updates)
    }

    /// Records a parameter the client announced in its startup packet.
    ///
    /// `application_name` is the one parameter PostgreSQL accepts there; it is
    /// reported back through `ParameterStatus` and `SHOW application_name`.
    pub fn set_parameter(&mut self, name: &str, value: String) {
        self.variables.insert(name.to_ascii_lowercase(), value);
    }

    /// Records a `ParameterStatus` update when the parameter is one PostgreSQL
    /// reports to the client.
    ///
    /// Only the reported set matters: a client caches exactly those values, so
    /// changing anything else is invisible to it.
    fn record_parameter_update(&mut self, name: &str, value: String) {
        if let Some((wire, _)) = REPORTED_PARAMETERS.iter().find(|(_, guc)| *guc == name) {
            self.parameter_updates.push(((*wire).to_string(), value));
        }
    }

    /// `SHOW ALL`: every parameter as `(name, setting, description)`, the shape
    /// PostgreSQL uses.
    fn show_all(&self) -> QueryResult {
        let rows = self
            .settings_snapshot()
            .into_iter()
            .map(|(name, setting)| {
                vec![
                    Value::Text(name),
                    Value::Text(setting),
                    Value::Text("VelocitySQL configuration parameter".to_string()),
                ]
            })
            .collect();
        QueryResult::Rows {
            schema: vec![
                Field::unqualified("name", DataType::Text),
                Field::unqualified("setting", DataType::Text),
                Field::unqualified("description", DataType::Text),
            ],
            rows,
        }
    }

    // -------------------------------------------------------------- helpers

    fn require_schema(&self, name: Option<&str>, default: &str) -> Result<SchemaDef, ExecError> {
        Ok(self
            .database
            .catalog
            .resolve_schema(name.unwrap_or(default))?)
    }

    fn require_table(&self, def: &Arc<TableDef>) -> Result<Arc<Table>, ExecError> {
        self.database
            .table(def.oid)
            .ok_or_else(|| ExecError::Catalog(CatalogError::UndefinedTable(def.name.clone())))
    }

    fn split_relation(&self, name: &[String]) -> Result<(Option<String>, String), ExecError> {
        match name {
            [] => Err(ExecError::other("empty relation name")),
            [relation] => Ok((None, relation.clone())),
            parts => {
                let relation = parts.last().cloned().unwrap_or_default();
                let schema = parts[parts.len() - 2].clone();
                Ok((Some(schema), relation))
            }
        }
    }

    /// The schema a DML statement sees for its target relation.
    fn relation_schema(&self, def: &Arc<TableDef>, alias: Option<&str>) -> Schema {
        let qualifier = alias
            .map(|alias| alias.to_string())
            .unwrap_or_else(|| def.name.clone());
        def.columns
            .iter()
            .filter(|column| !column.dropped)
            .map(|column| {
                Field::new(
                    Some(qualifier.clone()),
                    column.name.clone(),
                    column.data_type.clone(),
                )
            })
            .collect()
    }

    fn insert_targets(
        &self,
        def: &Arc<TableDef>,
        columns: &[String],
    ) -> Result<Vec<usize>, ExecError> {
        if columns.is_empty() {
            return Ok(def
                .columns
                .iter()
                .filter(|column| !column.dropped)
                .map(|column| column.ordinal)
                .collect());
        }
        let mut targets = Vec::with_capacity(columns.len());
        for name in columns {
            let index = def
                .column_index(name)
                .ok_or_else(|| ExecError::UndefinedColumn(name.clone()))?;
            targets.push(index);
        }
        Ok(targets)
    }

    fn default_row(&self, def: &Arc<TableDef>) -> Result<Row, ExecError> {
        let mut row = Vec::with_capacity(def.column_count());
        for column in &def.columns {
            row.push(self.default_for(column)?);
        }
        Ok(row)
    }

    /// Evaluates a column's `DEFAULT` expression.
    ///
    /// The catalog stores the expression as SQL text, so it is re-parsed and
    /// re-evaluated for every inserted row — which is exactly what makes
    /// `DEFAULT now()` advance instead of freezing at DDL time.
    fn default_for(&self, column: &ColumnDef) -> Result<Value, ExecError> {
        let Some(text) = &column.default else {
            return Ok(Value::Null);
        };
        let expr = vsql_parser::parse_expression(text)?;
        let mut planner = Planner::new(&self.database);
        let bound = planner.bind_expression(&expr)?;
        // Evaluated with the session's environment rather than
        // `eval_constant`, so a `DEFAULT nextval(...)` can reach the catalog.
        let value = bound.eval(&Row::new(), &self.environment())?;
        coerce(column, value)
    }

    #[allow(clippy::too_many_arguments)]
    fn attach_index(
        &self,
        table: &Arc<Table>,
        def: &Arc<TableDef>,
        name: String,
        columns: Vec<usize>,
        unique: bool,
        primary: bool,
        method: IndexMethod,
    ) -> Result<Arc<IndexDef>, ExecError> {
        if unique {
            self.check_unique_backfill(table, &name, &columns)?;
        }
        let index = IndexDef::new(
            self.database.catalog.next_oid(),
            def.oid,
            name,
            columns,
            method,
        );
        let index = if unique { index.unique() } else { index };
        let index = if primary { index.primary_key() } else { index };
        let index = self.database.catalog.register_index(index)?;
        table.attach_index(index.clone());
        Ok(index)
    }

    /// Rejects `CREATE UNIQUE INDEX` when the data already violates it.
    fn check_unique_backfill(
        &self,
        table: &Arc<Table>,
        name: &str,
        columns: &[usize],
    ) -> Result<(), ExecError> {
        let mut seen: HashSet<Vec<Value>> = HashSet::new();
        let mut violation = false;
        table.for_each(|_, row| {
            let mut key = Vec::with_capacity(columns.len());
            for column in columns {
                let value = row.get(*column).cloned().unwrap_or(Value::Null);
                if value.is_null() {
                    // PostgreSQL does not compare NULLs for uniqueness.
                    return true;
                }
                key.push(value);
            }
            if !seen.insert(key) {
                violation = true;
                return false;
            }
            true
        });
        if violation {
            return Err(ExecError::Storage(
                vsql_storage::StorageError::UniqueViolation {
                    index: name.to_string(),
                },
            ));
        }
        Ok(())
    }

    /// Resolves a `FOREIGN KEY` constraint against the catalog.
    ///
    /// The referenced table must already exist: PostgreSQL refuses a reference
    /// to a table created later, and so does this engine. `REFERENCES t`
    /// without a column list defaults to the target's primary key. The local
    /// columns come from `local_columns`, which is the table being created in
    /// the `CREATE TABLE` path and the resolved definition in `ALTER TABLE`.
    fn resolve_foreign_key(
        &self,
        table_name: &str,
        local_columns: &[ColumnDef],
        columns: &[String],
        ref_table: &[String],
        ref_columns: &[String],
        name: Option<&str>,
    ) -> Result<ForeignKeyDef, ExecError> {
        let local = self.resolve_columns(local_columns, columns)?;
        let ref_def = self.database.catalog.resolve_table(ref_table)?;
        let ref_cols = if ref_columns.is_empty() {
            if ref_def.primary_key.is_empty() {
                return Err(ExecError::other(format!(
                    "there is no primary key for referenced table \"{}\"",
                    ref_def.name
                )));
            }
            ref_def.primary_key.clone()
        } else {
            self.resolve_columns(&ref_def.columns, ref_columns)?
        };
        if local.len() != ref_cols.len() {
            return Err(ExecError::other(
                "number of referencing and referenced columns for foreign key mismatch",
            ));
        }
        for (&l, &r) in local.iter().zip(ref_cols.iter()) {
            if local_columns[l].data_type != ref_def.columns[r].data_type {
                return Err(ExecError::other(format!(
                    "foreign key constraint cannot be implemented: column \"{}\" and referenced column \"{}\" are of incompatible types",
                    local_columns[l].name, ref_def.columns[r].name
                )));
            }
        }
        let name = name
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}_{}_fkey", table_name, local_columns[local[0]].name));
        Ok(ForeignKeyDef {
            oid: self.database.catalog.next_oid(),
            name,
            columns: local,
            ref_table: ref_def.oid,
            ref_columns: ref_cols,
        })
    }

    /// Creates a sequence on `fallback_schema` when missing, so serial columns
    /// can be added repeatedly and `CREATE TABLE IF NOT EXISTS` stays
    /// idempotent.
    fn ensure_sequence(&self, fallback_schema: Oid, qualified: &str) -> Result<(), ExecError> {
        let (schema_oid, name) = match qualified.rsplit_once('.') {
            Some((schema, name)) => (
                self.database.catalog.resolve_schema(schema)?.oid,
                name.to_string(),
            ),
            None => (fallback_schema, qualified.to_string()),
        };
        self.database
            .catalog
            .create_sequence(schema_oid, &name, 1, 1, true)?;
        Ok(())
    }

    /// Parses and binds a table's `CHECK` constraints once, so a batch of rows
    /// can be validated without re-planning the expression per row.
    fn prepare_checks(&self, def: &Arc<TableDef>) -> Result<Vec<(String, ScalarExpr)>, ExecError> {
        if def.checks.is_empty() {
            return Ok(Vec::new());
        }
        let schema = self.relation_schema(def, None);
        let mut prepared = Vec::with_capacity(def.checks.len());
        for check in &def.checks {
            let expr = vsql_parser::parse_expression(&check.expr)?;
            let bound = Planner::new(&self.database).bind_expression_with_schema(&expr, &schema)?;
            prepared.push((check.name.clone(), bound));
        }
        Ok(prepared)
    }

    /// Rejects a row that violates one of `checks`, with PostgreSQL's 23514
    /// message.
    ///
    /// A result of `NULL` is UNKNOWN: as in PostgreSQL, a check constraint
    /// passes when it cannot prove the row wrong.
    fn check_row(
        &self,
        def: &Arc<TableDef>,
        checks: &[(String, ScalarExpr)],
        row: &Row,
    ) -> Result<(), ExecError> {
        if checks.is_empty() {
            return Ok(());
        }
        let env = self.environment();
        for (name, expr) in checks {
            match expr.eval(row, &env)? {
                Value::Bool(true) | Value::Null => {}
                Value::Bool(false) => {
                    return Err(ExecError::CheckViolation(format!(
                        "new row for relation \"{}\" violates check constraint \"{}\"",
                        def.name, name
                    )));
                }
                _ => {
                    return Err(ExecError::other(format!(
                        "argument of CHECK constraint \"{}\" must be type boolean",
                        name
                    )));
                }
            }
        }
        Ok(())
    }

    fn resolve_columns(
        &self,
        columns: &[ColumnDef],
        names: &[String],
    ) -> Result<Vec<usize>, ExecError> {
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let index = columns
                .iter()
                .position(|column| !column.dropped && column.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| ExecError::UndefinedColumn(name.clone()))?;
            out.push(index);
        }
        Ok(out)
    }

    fn column_names(&self, def: &Arc<TableDef>, columns: &[usize]) -> Vec<String> {
        columns
            .iter()
            .map(|index| def.columns[*index].name.clone())
            .collect()
    }

    fn insert_from_query(
        &mut self,
        def: &Arc<TableDef>,
        table: &Arc<Table>,
        query: &Query,
    ) -> Result<(), ExecError> {
        let plan = Planner::new(&self.database).plan_query(query)?;
        let targets: Vec<usize> = def
            .columns
            .iter()
            .filter(|column| !column.dropped)
            .map(|column| column.ordinal)
            .collect();
        if plan.schema().len() != targets.len() {
            return Err(ExecError::other(format!(
                "CREATE TABLE AS has {} columns but the query returns {}",
                targets.len(),
                plan.schema().len()
            )));
        }
        let checks = self.prepare_checks(def)?;
        for row in self.run_plan(&plan)? {
            let mut values = self.default_row(def)?;
            for (slot, value) in row.into_iter().enumerate() {
                values[targets[slot]] = coerce(&def.columns[targets[slot]], value)?;
            }
            self.check_row(def, &checks, &values)?;
            table.insert(values)?;
        }
        Ok(())
    }

    /// Binds a `RETURNING` list against the target relation's schema.
    fn bind_returning(
        &self,
        planner: &mut Planner<'_>,
        items: &[SelectItem],
        table_schema: &Schema,
    ) -> Result<(Vec<ScalarExpr>, Schema), ExecError> {
        let mut exprs = Vec::new();
        let mut schema = Schema::new();
        for item in items {
            match item {
                SelectItem::Wildcard => {
                    for (index, field) in table_schema.iter().enumerate() {
                        exprs.push(ScalarExpr::Column(index));
                        schema.push(field.clone());
                    }
                }
                SelectItem::QualifiedWildcard(_) => {
                    for (index, field) in table_schema.iter().enumerate() {
                        exprs.push(ScalarExpr::Column(index));
                        schema.push(field.clone());
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    let bound = planner.bind_expression_with_schema(expr, table_schema)?;
                    let data_type = bound.infer_type(table_schema).unwrap_or(DataType::Text);
                    let name = alias.clone().unwrap_or_else(|| match expr {
                        Expr::Column(reference) => reference.name.clone(),
                        _ => "?column?".to_string(),
                    });
                    schema.push(Field::new(None, name, data_type));
                    exprs.push(bound);
                }
            }
        }
        Ok((exprs, schema))
    }

    fn project_rows(
        &self,
        exprs: &[ScalarExpr],
        rows: &[Row],
        env: &EvalEnv,
    ) -> Result<Vec<Row>, ExecError> {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut projected = Vec::with_capacity(exprs.len());
            for expr in exprs {
                projected.push(expr.eval(row, env)?);
            }
            out.push(projected);
        }
        Ok(out)
    }
}

/// The sequence a `DEFAULT nextval('...')` expression advances, when the
/// default is such a call over a string literal.
fn default_sequence(default: &Option<Expr>) -> Option<String> {
    let Expr::Function(call) = default.as_ref()? else {
        return None;
    };
    if !call.name.eq_ignore_ascii_case("nextval") {
        return None;
    }
    match call.args.first() {
        Some(Expr::Literal(Value::Text(name))) => Some(name.clone()),
        _ => None,
    }
}

/// Whether a statement changes data, schema or roles, and so should be followed
/// by a persistence snapshot. `SELECT`, `SHOW`, `EXPLAIN`, `SET` and
/// transaction bookkeeping are read-only from the snapshot's point of view.
fn is_mutating_statement(statement: &Statement) -> bool {
    matches!(
        statement,
        Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
            | Statement::CreateTable(_)
            | Statement::CreateIndex(_)
            | Statement::CreateView(_)
            | Statement::CreateSchema { .. }
            | Statement::AlterTable(_)
            | Statement::DropTable { .. }
            | Statement::CreateRole { .. }
            | Statement::AlterRole { .. }
            | Statement::DropRole { .. }
            | Statement::CreateDatabase { .. }
            | Statement::DropDatabase { .. }
            | Statement::Truncate { .. }
            | Statement::Commit
    )
}

/// Derives a unique constraint name from `base`, appending a number the way
/// PostgreSQL does when a table already has a constraint of that name.
fn unique_check_name(base: &str, used: &mut HashSet<String>) -> String {
    let mut name = base.to_string();
    let mut n = 0;
    while used.contains(&name) {
        n += 1;
        name = format!("{base}{n}");
    }
    used.insert(name.clone());
    name
}

/// A table's layout while `ALTER TABLE` is editing it.
///
/// Every operation works on a copy and hands the finished layout back to the
/// catalog, so a statement that is rejected half way through never leaves a table
/// in a shape that no operation asked for.
struct TableLayout {
    name: String,
    owner: Option<String>,
    columns: Vec<ColumnDef>,
    primary_key: Vec<usize>,
}

impl TableLayout {
    /// Copies the layout of `def`.
    fn of(def: &TableDef) -> TableLayout {
        TableLayout {
            name: def.name.clone(),
            owner: def.owner.clone(),
            columns: def.columns.clone(),
            primary_key: def.primary_key.clone(),
        }
    }

    /// Builds the definition this layout describes, keeping `old`'s identity.
    ///
    /// The OID, the schema and the system flag are the table's; the name, the
    /// columns, the primary key and the owner are what `ALTER TABLE` was asked to
    /// change.
    fn into_def(self, old: &TableDef) -> Arc<TableDef> {
        let mut def = TableDef::new(old.oid, old.schema_oid, self.name, self.columns);
        def.primary_key = self.primary_key;
        def.owner = self.owner;
        def.is_system = old.is_system;
        Arc::new(def)
    }

    /// The ordinal of a live column, or an error naming it.
    fn column(&self, name: &str) -> Result<usize, ExecError> {
        self.column_index(name)
            .ok_or_else(|| ExecError::UndefinedColumn(name.to_string()))
    }

    /// The ordinal of a live column, by name.
    fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| !column.dropped && column.name.eq_ignore_ascii_case(name))
    }

    /// `true` when the layout already has a live column with this name.
    fn has_column(&self, name: &str) -> bool {
        self.column_index(name).is_some()
    }
}
/// Converts a value to a column's declared type.
///
/// `NOT NULL` is enforced by the storage engine, so a `NULL` here is passed
/// through unchanged rather than rejected twice.
fn coerce(column: &ColumnDef, value: Value) -> Result<Value, ExecError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    Ok(value.cast_to(&column.data_type)?)
}

/// Renders the value of a `SET` assignment the way PostgreSQL would store it.
fn render_set_value(value: &Expr) -> String {
    match value {
        Expr::Literal(text) => text.to_string(),
        other => render::ast_expr(other),
    }
}

fn isolation_name(isolation: IsolationLevel) -> &'static str {
    match isolation {
        IsolationLevel::ReadUncommitted => "read uncommitted",
        IsolationLevel::ReadCommitted => "read committed",
        IsolationLevel::RepeatableRead => "repeatable read",
        IsolationLevel::Serializable => "serializable",
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Releases the connection slot that `DROP DATABASE` checks, so a later
        // `DROP DATABASE` is not blocked by a session that has gone away.
        self.database.detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Engine;

    #[test]
    fn bind_parameters_drive_comparisons() {
        let engine = Arc::new(Engine::new());
        let mut session = Session::new(engine);
        session
            .execute("CREATE TABLE carts(id int, token text)")
            .unwrap();
        session
            .execute("INSERT INTO carts VALUES (1, 'a1'), (2, 'b2')")
            .unwrap();

        // `$1` resolves from the parameter list at evaluation time.
        let result = session
            .execute_with_params(
                "SELECT id FROM carts WHERE token = $1",
                &[Value::Text("b2".into())],
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = &result[0] else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int4(2));

        // A non-string parameter adopted by an implicit cast (int = $1).
        let result = session
            .execute_with_params("SELECT token FROM carts WHERE id = $1", &[Value::Int4(1)])
            .unwrap();
        let QueryResult::Rows { rows, .. } = &result[0] else {
            panic!("expected rows");
        };
        assert_eq!(rows[0][0], Value::Text("a1".into()));
    }

    #[test]
    fn any_reads_an_untyped_array_literal_like_postgres() {
        let engine = Arc::new(Engine::new());
        let mut session = Session::new(engine);
        session.execute("CREATE TABLE t(id int)").unwrap();
        session
            .execute("INSERT INTO t VALUES (1), (2), (3)")
            .unwrap();

        // PostgreSQL coerces the untyped literal to the left operand's array
        // type, so `id = ANY('{1,2}')` matches both rows.
        let result = session
            .execute("SELECT id FROM t WHERE id = ANY('{1,2}') ORDER BY id")
            .unwrap();
        let QueryResult::Rows { rows, .. } = &result[0] else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Int4(1));
        assert_eq!(rows[1][0], Value::Int4(2));

        // The same through a bound array parameter, as a driver sends it.
        let result = session
            .execute_with_params(
                "SELECT id FROM t WHERE id = ANY($1) ORDER BY id",
                &[Value::Array(vec![
                    Value::Text("2".into()),
                    Value::Text("3".into()),
                ])],
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = &result[0] else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Int4(2));
        assert_eq!(rows[1][0], Value::Int4(3));

        // The same text form, for the one function that can only want an array.
        let result = session
            .execute_with_params(
                "SELECT * FROM unnest($1) ORDER BY 1",
                &[Value::Text("{2,3}".into())],
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = &result[0] else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Text("2".into()));
        assert_eq!(rows[1][0], Value::Text("3".into()));
    }
}
