//! Databases and the engine that owns them.
//!
//! # Hierarchy
//!
//! ```text
//! Engine (one per process, i.e. the cluster)
//!   └── Database  "velocitysql", "postgres", ...
//!         └── Catalog   +  Storage  +  views      <- one set per database
//!               └── Schema  ->  Table / Index / Sequence
//! ```
//!
//! This mirrors PostgreSQL: `CREATE DATABASE` creates a namespace whose objects
//! cannot be reached from another database, and switching databases needs a new
//! connection (`\c` in psql), not a `SET`.
//!
//! # Why the registry lives here and not in `vsql-catalog`
//!
//! `vsql-catalog` describes a database (`DatabaseDef`: OID, name, owner) but
//! cannot *own* one, because owning one means holding a [`Storage`], and
//! `vsql-storage` depends on `vsql-catalog`. The composition therefore belongs to
//! this crate: [`Database`] pairs a catalog with the rows it describes, and
//! [`Engine`] plays the role of the cluster that holds them.
//!
//! # OIDs
//!
//! Relation OIDs are allocated per database (`Catalog::next_oid`), which is what
//! PostgreSQL does — `pg_class.oid` is unique within a database, not across a
//! cluster. Database OIDs are allocated by the engine and *are* cluster-wide
//! unique, matching `pg_database`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use vsql_catalog::{oids, Catalog, CatalogError, DatabaseDef, Oid, RoleDef, TableDef};
use vsql_parser::ast::Query;
use vsql_parser::RoleOption;
use vsql_storage::{Storage, Table};

use crate::error::ExecError;

/// The database a session connects to when it does not ask for one.
pub const DEFAULT_DATABASE: &str = "velocitysql";

/// Owner assigned to databases created without an explicit role.
pub const DEFAULT_OWNER: &str = "velocitysql";

/// Name of the compatibility database.
///
/// Countless scripts, tutorials and tools hardcode `postgres`; creating it up
/// front costs nothing and avoids a confusing `3D000` on a healthy server.
pub const COMPAT_DATABASE: &str = "postgres";

/// PostgreSQL's `NAMEDATALEN - 1`: identifiers longer than this are rejected.
const MAX_IDENTIFIER_BYTES: usize = 63;

/// One database: its own catalog, its own rows, its own views.
#[derive(Debug)]
pub struct Database {
    def: DatabaseDef,
    /// Schemas, relations, indexes and sequences of this database.
    pub catalog: Catalog,
    /// The rows those relations describe.
    pub storage: Storage,
    /// Views, keyed by `(schema oid, view name)`.
    views: RwLock<HashMap<(Oid, String), Arc<Query>>>,
    /// Number of sessions currently attached.
    ///
    /// `DROP DATABASE` refuses while this is non-zero: a session that kept
    /// running against a dropped database would silently diverge from the
    /// catalog, which is exactly what PostgreSQL prevents with
    /// `database "x" is being accessed by other users`.
    sessions: AtomicUsize,
}

impl Database {
    /// Creates an empty database with PostgreSQL's default schemas.
    pub fn new(def: DatabaseDef) -> Arc<Database> {
        Arc::new(Database {
            def,
            catalog: Catalog::new(),
            storage: Storage::new(),
            views: RwLock::new(HashMap::new()),
            sessions: AtomicUsize::new(0),
        })
    }

    /// The database's metadata.
    pub fn def(&self) -> &DatabaseDef {
        &self.def
    }

    /// The database's name.
    pub fn name(&self) -> &str {
        &self.def.name
    }

    /// The database's OID (`pg_database.oid`).
    pub fn oid(&self) -> Oid {
        self.def.oid
    }

    // ------------------------------------------------------------- relations

    /// Returns the mounted table for `oid`.
    pub fn table(&self, oid: Oid) -> Option<Arc<Table>> {
        self.storage.table(oid)
    }

    /// Mounts a relation described by `def`.
    pub fn mount(&self, def: Arc<TableDef>) -> Arc<Table> {
        match self.storage.table(def.oid) {
            Some(table) => table,
            None => self.storage.create_table(def),
        }
    }

    // ----------------------------------------------------------------- views

    /// Resolves a view by (possibly qualified) name.
    pub fn view(&self, name: &[String]) -> Option<Arc<Query>> {
        let relation = name.last()?.to_ascii_lowercase();
        let views = self.views.read();
        match name.len() {
            1 => ["public", "pg_catalog"].iter().find_map(|schema| {
                let schema = self.catalog.schema(schema)?;
                views.get(&(schema.oid, relation.clone())).cloned()
            }),
            _ => {
                let schema = self.catalog.schema(&name[name.len() - 2])?;
                views.get(&(schema.oid, relation)).cloned()
            }
        }
    }

    /// Registers a view definition.
    pub fn create_view(
        &self,
        schema_oid: Oid,
        name: &str,
        query: Query,
        or_replace: bool,
    ) -> Result<(), ExecError> {
        let name = name.to_ascii_lowercase();
        let mut views = self.views.write();
        if views.contains_key(&(schema_oid, name.clone())) && !or_replace {
            return Err(ExecError::Catalog(CatalogError::RelationExists(name)));
        }
        views.insert((schema_oid, name), Arc::new(query));
        Ok(())
    }

    /// View names inside a schema, sorted.
    pub fn views_in(&self, schema_oid: Oid) -> Vec<String> {
        let mut names: Vec<String> = self
            .views
            .read()
            .keys()
            .filter(|(schema, _)| *schema == schema_oid)
            .map(|(_, name)| name.clone())
            .collect();
        names.sort();
        names
    }

    // -------------------------------------------------------------- sessions

    /// Records that a session connected.
    pub fn attach(&self) {
        self.sessions.fetch_add(1, Ordering::SeqCst);
    }

    /// Records that a session disconnected.
    pub fn detach(&self) {
        // Saturating: a double detach (only possible through a bug) must not
        // wrap the counter to `usize::MAX` and block every future `DROP`.
        let _ = self
            .sessions
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.saturating_sub(1))
            });
    }

    /// Number of attached sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.load(Ordering::SeqCst)
    }
}

/// The bootstrap superuser, created by [`Engine::new`].
pub const DEFAULT_SUPERUSER: &str = "velocitysql";

/// Password given to the bootstrap superuser unless one is supplied.
///
/// It is documented rather than secret on purpose: a default nobody can find is
/// a default nobody changes. The server warns while it is still in use.
pub const DEFAULT_PASSWORD: &str = "velocitysql";

/// A callback the operator installs to be told when data changed, so an
/// asynchronous snapshot can follow the write.
#[derive(Clone)]
pub struct PersistHook(Arc<dyn Fn() + Send + Sync>);

impl PersistHook {
    /// Wraps `callback`, which is called after every successful
    /// data-modifying statement.
    pub fn new(callback: impl Fn() + Send + Sync + 'static) -> Self {
        PersistHook(Arc::new(callback))
    }
}

impl std::fmt::Debug for PersistHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PersistHook(<callback>)")
    }
}

/// The cluster: every database and every role in this process.
#[derive(Debug)]
pub struct Engine {
    databases: RwLock<HashMap<String, Arc<Database>>>,
    /// Roles, keyed by folded name.
    ///
    /// Roles live at cluster level like PostgreSQL's `pg_authid`: a `CREATE ROLE`
    /// in one database is visible from all the others.
    roles: RwLock<HashMap<String, RoleDef>>,
    /// OID source for cluster-level objects (`pg_database`, `pg_authid`).
    next_oid: AtomicU32,
    /// Called after a successful data-modifying statement; never set in tests
    /// that do not need persistence.
    persist_hook: RwLock<Option<PersistHook>>,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

impl Engine {
    /// Creates the cluster with its bootstrap superuser and default databases.
    pub fn new() -> Self {
        Engine::with_superuser_password(DEFAULT_PASSWORD)
    }

    /// Creates the cluster, giving the bootstrap superuser `password`.
    pub fn with_superuser_password(password: &str) -> Self {
        let engine = Engine {
            databases: RwLock::new(HashMap::new()),
            roles: RwLock::new(HashMap::new()),
            next_oid: AtomicU32::new(oids::FIRST_USER_OID),
            persist_hook: RwLock::new(None),
        };
        let mut superuser = RoleDef::new(
            engine.next_oid.fetch_add(1, Ordering::SeqCst),
            DEFAULT_SUPERUSER,
        )
        .superuser()
        .login()
        .create_db()
        .create_role();
        superuser.set_password(password);
        engine
            .roles
            .write()
            .insert(DEFAULT_SUPERUSER.to_string(), superuser);

        // Infallible on a fresh engine: the names are valid and unused.
        for name in [DEFAULT_DATABASE, COMPAT_DATABASE] {
            engine
                .create_database(name, DEFAULT_OWNER, false)
                .expect("a fresh engine accepts its own default databases");
        }
        engine
    }

    /// The name of the bootstrap superuser.
    pub fn superuser_name(&self) -> &'static str {
        DEFAULT_SUPERUSER
    }

    /// Installs the callback invoked after every successful data-modifying
    /// statement. Only one hook is kept; calling this again replaces it.
    pub fn set_persist_hook(&self, hook: PersistHook) {
        *self.persist_hook.write() = Some(hook);
    }

    /// Fires the persistence callback, if one is installed.
    ///
    /// Called by a session after a mutating statement has been applied in
    /// memory and its result is on its way back: the write is never delayed by
    /// whatever the hook does.
    pub(crate) fn notify_mutation(&self) {
        if let Some(hook) = self.persist_hook.read().as_ref() {
            (hook.0)();
        }
    }

    /// Raises the cluster's OID source to `at_least`.
    ///
    /// Used by a snapshot restore, which installs roles and databases that
    /// carried their own OIDs: the next `CREATE` must allocate past everything
    /// already on disk, or a fresh object would collide with a restored one.
    pub fn bump_next_oid(&self, at_least: Oid) {
        let _ = self
            .next_oid
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.max(at_least))
            });
    }

    /// The name of the database a bare connection lands in.
    pub fn default_database_name() -> &'static str {
        DEFAULT_DATABASE
    }

    /// Looks up a database by name (case-insensitively).
    pub fn database(&self, name: &str) -> Option<Arc<Database>> {
        let name = name.trim().to_ascii_lowercase();
        self.databases.read().get(&name).cloned()
    }

    /// Looks up a database, reporting `3D000` when it is unknown.
    pub fn require_database(&self, name: &str) -> Result<Arc<Database>, CatalogError> {
        self.database(name)
            .ok_or_else(|| CatalogError::UndefinedDatabase(name.to_string()))
    }

    /// The default database.
    pub fn default_database(&self) -> Arc<Database> {
        self.database(DEFAULT_DATABASE)
            .expect("the default database is created by `Engine::new`")
    }

    /// Every database, ordered by name.
    pub fn databases(&self) -> Vec<Arc<Database>> {
        let mut databases: Vec<Arc<Database>> = self.databases.read().values().cloned().collect();
        databases.sort_by(|left, right| left.name().cmp(right.name()));
        databases
    }

    /// Creates a database.
    pub fn create_database(
        &self,
        name: &str,
        owner: &str,
        if_not_exists: bool,
    ) -> Result<Arc<Database>, CatalogError> {
        let name = validate_database_name(name)?;
        let mut databases = self.databases.write();
        if let Some(existing) = databases.get(&name) {
            if if_not_exists {
                return Ok(existing.clone());
            }
            return Err(CatalogError::DatabaseExists(name));
        }
        let def = DatabaseDef::new(
            self.next_oid.fetch_add(1, Ordering::SeqCst),
            name.clone(),
            owner,
        );
        let database = Database::new(def);
        databases.insert(name, database.clone());
        Ok(database)
    }

    /// Installs a database restored from a snapshot under its own OID.
    ///
    /// Unlike [`Engine::create_database`], which mints a fresh OID, this keeps
    /// the OID the snapshot carried (so `pg_database.oid` is stable across
    /// restarts) and returns the existing database when one of the same name is
    /// already present, making restore idempotent against the defaults created
    /// by [`Engine::new`]. The caller is responsible for raising the OID source
    /// through [`Engine::bump_next_oid`] once every object is installed.
    pub fn install_database(&self, def: DatabaseDef) -> Arc<Database> {
        let mut databases = self.databases.write();
        if let Some(existing) = databases.get(&def.name) {
            return existing.clone();
        }
        let database = Database::new(def.clone());
        databases.insert(def.name, database.clone());
        database
    }

    /// Drops a database.
    ///
    /// `currently_open` is the name of the database the calling session is
    /// connected to; PostgreSQL refuses to drop it, and so do we.
    pub fn drop_database(
        &self,
        name: &str,
        if_exists: bool,
        currently_open: Option<&str>,
    ) -> Result<(), CatalogError> {
        let name = name.trim().to_ascii_lowercase();
        if currently_open.map(|open| open == name).unwrap_or(false) {
            return Err(CatalogError::CannotDropCurrentDatabase);
        }
        let mut databases = self.databases.write();
        let Some(database) = databases.get(&name) else {
            return if if_exists {
                Ok(())
            } else {
                Err(CatalogError::UndefinedDatabase(name))
            };
        };
        if database.session_count() > 0 {
            return Err(CatalogError::DatabaseInUse(name));
        }
        databases.remove(&name);
        Ok(())
    }

    // ------------------------------------------------------------------ roles

    /// Looks up a role by name (case-insensitively).
    pub fn role(&self, name: &str) -> Option<RoleDef> {
        let name = name.trim().to_ascii_lowercase();
        self.roles.read().get(&name).cloned()
    }

    /// Every role, ordered by name.
    pub fn roles(&self) -> Vec<RoleDef> {
        let mut roles: Vec<RoleDef> = self.roles.read().values().cloned().collect();
        roles.sort_by(|left, right| left.name.cmp(&right.name));
        roles
    }

    /// Establishes that `user` exists and may log in, without checking a
    /// password. Used by `trust` authentication and by in-process callers.
    pub fn identify(&self, user: &str) -> Result<RoleDef, CatalogError> {
        let role = self
            .role(user)
            .ok_or_else(|| CatalogError::UndefinedRole(user.to_string()))?;
        if !role.can_login {
            return Err(CatalogError::RoleCannotLogin(role.name));
        }
        Ok(role)
    }

    /// Authenticates `user` with `password`.
    ///
    /// An unknown role and a wrong password produce the *same* error on purpose:
    /// telling them apart would let anyone enumerate the cluster's accounts. The
    /// distinction is available to operators through the server log instead.
    pub fn authenticate(
        &self,
        user: &str,
        password: Option<&str>,
    ) -> Result<RoleDef, CatalogError> {
        let failed = || CatalogError::PasswordAuthenticationFailed(user.to_string());
        let role = self.role(user).ok_or_else(failed)?;
        if !role.can_login {
            return Err(CatalogError::RoleCannotLogin(role.name));
        }
        // A role without a password cannot log in with one.
        let (Some(verifier), Some(password)) = (role.verifier(), password) else {
            return Err(failed());
        };
        if verifier.verify_password(password) {
            Ok(role)
        } else {
            Err(failed())
        }
    }

    /// Creates a role.
    pub fn create_role(
        &self,
        name: &str,
        options: &[RoleOption],
        if_not_exists: bool,
    ) -> Result<RoleDef, CatalogError> {
        let name = vsql_catalog::normalise_identifier(name)?;
        let mut roles = self.roles.write();
        if let Some(existing) = roles.get(&name) {
            if if_not_exists {
                return Ok(existing.clone());
            }
            return Err(CatalogError::RoleExists(name));
        }
        let mut role = RoleDef::new(self.next_oid.fetch_add(1, Ordering::SeqCst), name.clone());
        apply_role_options(&mut role, options);
        roles.insert(name, role.clone());
        Ok(role)
    }

    /// Installs a role restored from a snapshot, keeping its OID and verifier.
    ///
    /// Returns the stored role: an existing role of the same name wins, so
    /// restoring a snapshot never clobbers a role the process already has (its
    /// bootstrap superuser, whose password comes from the command line).
    pub fn install_role(&self, role: RoleDef) -> RoleDef {
        let mut roles = self.roles.write();
        if let Some(existing) = roles.get(&role.name) {
            return existing.clone();
        }
        roles.insert(role.name.clone(), role.clone());
        role
    }

    /// Changes a role's attributes.
    pub fn alter_role(&self, name: &str, options: &[RoleOption]) -> Result<RoleDef, CatalogError> {
        let name = name.trim().to_ascii_lowercase();
        let mut roles = self.roles.write();
        if name == DEFAULT_SUPERUSER && drops_superuser(options) {
            return Err(CatalogError::CannotDropSuperuser(name));
        }
        let role = roles
            .get_mut(&name)
            .ok_or_else(|| CatalogError::UndefinedRole(name.clone()))?;
        apply_role_options(role, options);
        Ok(role.clone())
    }

    /// Drops a role.
    pub fn drop_role(
        &self,
        name: &str,
        if_exists: bool,
        acting_as: Option<&str>,
    ) -> Result<(), CatalogError> {
        let name = name.trim().to_ascii_lowercase();
        if acting_as.map(|current| current == name).unwrap_or(false) {
            return Err(CatalogError::CannotDropCurrentRole);
        }
        if name == DEFAULT_SUPERUSER {
            return Err(CatalogError::CannotDropSuperuser(name));
        }
        let mut roles = self.roles.write();
        if roles.remove(&name).is_none() && !if_exists {
            return Err(CatalogError::UndefinedRole(name));
        }
        Ok(())
    }
}

/// `true` when the options would revoke `SUPERUSER`.
fn drops_superuser(options: &[RoleOption]) -> bool {
    options
        .iter()
        .any(|option| matches!(option, RoleOption::Superuser(false)))
}

/// Applies `CREATE ROLE` / `ALTER ROLE` attributes to a role.
fn apply_role_options(role: &mut RoleDef, options: &[RoleOption]) {
    for option in options {
        match option {
            RoleOption::Superuser(value) => role.superuser = *value,
            RoleOption::Login(value) => role.can_login = *value,
            RoleOption::CreateDb(value) => role.create_db = *value,
            RoleOption::CreateRole(value) => role.create_role = *value,
            RoleOption::Password(Some(password)) => role.set_password(password),
            RoleOption::Password(None) => role.clear_password(),
        }
    }
}

/// Normalises and validates a database name.
///
/// PostgreSQL folds unquoted identifiers to lower case and truncates anything
/// longer than 63 bytes with a notice. VelocitySQL folds identically (the parser
/// has already done it for SQL text, and this covers names arriving through the
/// startup packet) but *rejects* over-long names instead of silently truncating:
/// a database that cannot be named back is worse than a rejected one.
fn validate_database_name(name: &str) -> Result<String, CatalogError> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return Err(CatalogError::Other("database name cannot be empty".into()));
    }
    if name.len() > MAX_IDENTIFIER_BYTES {
        return Err(CatalogError::IdentifierTooLong(name));
    }
    if name.starts_with("pg_") {
        return Err(CatalogError::ReservedDatabaseName(name));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_engine_ships_two_databases() {
        let engine = Engine::new();
        let names: Vec<String> = engine
            .databases()
            .iter()
            .map(|database| database.name().to_string())
            .collect();
        assert!(names.iter().any(|name| name == DEFAULT_DATABASE));
        assert!(names.iter().any(|name| name == COMPAT_DATABASE));
        assert!(engine.database("POSTGRES").is_some(), "lookup folds case");
    }

    #[test]
    fn database_oids_are_cluster_wide_and_unique() {
        let engine = Engine::new();
        let left = engine.create_database("alpha", "tester", false).unwrap();
        let right = engine.create_database("beta", "tester", false).unwrap();
        assert_ne!(left.oid(), right.oid());
        assert!(left.oid() >= oids::FIRST_USER_OID);
    }

    #[test]
    fn names_are_validated() {
        let engine = Engine::new();
        let error = engine
            .create_database("pg_reserved", "tester", false)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unacceptable database name \"pg_reserved\""
        );

        let long = "x".repeat(64);
        let error = engine.create_database(&long, "tester", false).unwrap_err();
        assert!(
            matches!(error, CatalogError::IdentifierTooLong(_)),
            "{error}"
        );

        // 63 bytes is fine.
        assert!(engine
            .create_database(&"y".repeat(63), "tester", false)
            .is_ok());
    }

    #[test]
    fn create_is_idempotent_with_if_not_exists() {
        let engine = Engine::new();
        let first = engine.create_database("shop", "tester", false).unwrap();
        let again = engine.create_database("shop", "tester", true).unwrap();
        assert_eq!(first.oid(), again.oid());
        let error = engine.create_database("shop", "tester", false).unwrap_err();
        assert_eq!(error.to_string(), "database \"shop\" already exists");
    }

    #[test]
    fn dropping_refuses_the_open_database_and_databases_in_use() {
        let engine = Engine::new();
        engine.create_database("shop", "tester", false).unwrap();

        let error = engine
            .drop_database("shop", false, Some("shop"))
            .unwrap_err();
        assert_eq!(error.to_string(), "cannot drop the currently open database");

        let shop = engine.database("shop").unwrap();
        shop.attach();
        let error = engine.drop_database("shop", false, None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "database \"shop\" is being accessed by other users"
        );

        shop.detach();
        engine.drop_database("shop", false, None).unwrap();
        assert!(engine.database("shop").is_none());
        engine.drop_database("shop", true, None).unwrap();
        assert!(engine
            .drop_database("shop", false, None)
            .unwrap_err()
            .to_string()
            .contains("does not exist"));
    }

    #[test]
    fn detach_does_not_underflow() {
        let engine = Engine::new();
        let database = engine.default_database();
        database.attach();
        database.detach();
        // A second detach must not wrap the counter to `usize::MAX`.
        database.detach();
        assert_eq!(database.session_count(), 0);
    }
}
