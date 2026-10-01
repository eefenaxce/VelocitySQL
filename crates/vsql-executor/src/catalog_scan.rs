//! Virtual `pg_catalog` relations.
//!
//! PostgreSQL keeps its catalogue in real tables; VelocitySQL materialises the
//! commonly read ones on demand from live metadata. The rows therefore can never
//! drift from what the engine will actually do - a `pg_roles` row exists because
//! the role exists, not because a catalog table was updated in step.
//!
//! # What is here
//!
//! `pg_database`, `pg_roles`, `pg_user`, `pg_namespace`, `pg_class`,
//! `pg_attribute`, `pg_attrdef`, `pg_type`, `pg_settings`, `pg_tables`,
//! `pg_views`, `pg_indexes`, `pg_tablespace`, `pg_constraint`, `pg_am`,
//! `pg_index`, `pg_sequence` and `pg_sequences` - the relations GUI tools and
//! ORMs query while connecting or
//! browsing. Columns
//! follow PostgreSQL's names and types; columns that would require features the
//! engine does not have (`datacl`, view definitions) are `NULL`/empty rather
//! than wrong.
//!
//! `EMPTY_RELATIONS` holds the relations the engine recognises but has no
//! objects for yet (foreign tables, publications, ...): they exist with their
//! real column lists so a tool that joins them receives an empty set instead of
//! an error.
//!
//! # What is deliberately *not* here
//!
//! * `pg_attribute` covers tables only: views have no relation OID yet, so their
//!   attributes cannot be joined consistently.
//! * `pg_class` lists views with a **synthetic OID** (a hash of the qualified
//!   name in a reserved range); it is stable within and across queries, but it
//!   is not the OID of any storage.
//! * `pg_views.definition` is empty until the planner learns to render SQL back.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use vsql_catalog::{Catalog, DatabaseDef, IndexDef, IndexMethod, Oid, RoleDef, TableDef};
use vsql_types::{DataType, Value};

use crate::database::{Database, Engine};
use crate::field::{Field, Schema};
use crate::render;

/// The namespace OID of `pg_catalog`, as PostgreSQL reserves it.
const PG_CATALOG_NAMESPACE: i32 = 11;

/// Access-method OIDs from `pg_am`, as PostgreSQL assigns them.
const BTREE_AM: i32 = 403;
const HASH_AM: i32 = 405;

/// Live cluster metadata, snapshotted once per statement.
///
/// The snapshot is what keeps a single query consistent: `pg_class` and
/// `pg_attribute` from the same statement describe the same cluster state even
/// if a concurrent `CREATE TABLE` lands in between.
pub struct CatalogSnapshot {
    /// The database the session is attached to.
    pub database: Arc<Database>,
    /// Every database in the cluster, for `pg_database`.
    pub databases: Vec<DatabaseDef>,
    /// Every role in the cluster, for `pg_roles`/`pg_user`/`pg_get_userbyid`.
    pub roles: Vec<RoleDef>,
    /// OID of the bootstrap superuser, the owner of all metadata objects.
    pub superuser_oid: Oid,
    /// Every configuration parameter as `(name, setting)`, for `pg_settings`.
    pub settings: Vec<(String, String)>,
    /// The session's `search_path`, for `current_schema()`.
    pub search_path: String,
}

impl CatalogSnapshot {
    /// The role named `name`, if it exists.
    fn role_named(&self, name: &str) -> Option<&RoleDef> {
        self.roles.iter().find(|role| role.name == name)
    }

    /// The OID a role name maps to, or `0` - PostgreSQL's "invalid OID".
    fn owner_oid_of(&self, name: &str) -> Oid {
        self.role_named(name).map(|role| role.oid).unwrap_or(0)
    }

    /// The owner every metadata object is reported with.
    ///
    /// Object ownership is not tracked per role yet; the bootstrap superuser is
    /// the truthful answer for everything the engine creates itself.
    fn owner_oid(&self) -> Oid {
        self.superuser_oid
    }

    /// The owner OID of a relation, falling back to the bootstrap superuser.
    ///
    /// A role that `OWNER TO` named and that has since been dropped keeps
    /// PostgreSQL's "invalid OID" answer rather than silently changing hands.
    fn relation_owner_oid(&self, owner: Option<&str>) -> Oid {
        match owner {
            Some(name) => self.owner_oid_of(name),
            None => self.superuser_oid,
        }
    }

    /// The owner of a relation as a *name*, for the `*owner` columns of
    /// `pg_tables` & co.
    fn relation_owner_name(&self, owner: Option<&str>) -> String {
        match owner {
            Some(name) => name.to_string(),
            None => self.owner_name(),
        }
    }

    /// The owner as a *name*, for the `*owner` columns of `pg_tables` & co.
    fn owner_name(&self) -> String {
        self.roles
            .iter()
            .find(|role| role.oid == self.superuser_oid)
            .map(|role| role.name.clone())
            .unwrap_or_else(|| "velocitysql".to_string())
    }

    /// The schema name for `oid`, or an empty string when unknown.
    fn schema_name(&self, oid: Oid) -> String {
        self.database
            .catalog
            .schemas()
            .iter()
            .find(|schema| schema.oid == oid)
            .map(|schema| schema.name.clone())
            .unwrap_or_default()
    }

    /// The first search-path entry that is not the `$user` placeholder.
    ///
    /// That is what `current_schema()` reports in PostgreSQL: the first schema
    /// *that exists*, and `$user` only counts when a schema named after the role
    /// exists - which VelocitySQL never creates.
    pub fn current_schema(&self) -> String {
        self.search_path
            .split(',')
            .map(|name| name.trim().trim_matches('"').to_string())
            .find(|name| !name.is_empty() && name != "$user")
            .unwrap_or_else(|| "public".to_string())
    }

    /// The schema names `current_schemas(include_implicit)` reports.
    ///
    /// Every search-path entry, with the `$user` placeholder dropped like
    /// `current_schema()` drops it; `include_implicit` prepends `pg_catalog`,
    /// which PostgreSQL always searches first.
    pub fn current_schemas(&self, include_implicit: bool) -> Vec<String> {
        let mut schemas: Vec<String> = self
            .search_path
            .split(',')
            .map(|name| name.trim().trim_matches('"').to_string())
            .filter(|name| !name.is_empty() && name != "$user")
            .collect();
        if include_implicit {
            schemas.insert(0, "pg_catalog".to_string());
        }
        schemas
    }

    /// The role with `oid`, if it exists.
    pub fn role_name(&self, oid: Oid) -> Option<&str> {
        self.roles
            .iter()
            .find(|role| role.oid == oid)
            .map(|role| role.name.as_str())
    }
}

impl CatalogSnapshot {
    /// A `pg_class` row in PostgreSQL's column order.
    ///
    /// Every relation kind shares the shape: a table (`r`), an index (`i`, whose
    /// `relam` names its access method) or a view (`v`). Values the engine cannot
    /// know yet (statistics, ACLs, frozen-xid bookkeeping) take PostgreSQL's
    /// empty representation.
    fn class_row(&self, relation: ClassRelation<'_>) -> Vec<Value> {
        let ClassRelation {
            oid,
            name,
            namespace_oid,
            kind,
            relam,
            natts,
            has_index,
            owner,
        } = relation;
        vec![
            Value::Int4(oid as i32),
            Value::Text(name.to_string()),
            Value::Int4(namespace_oid as i32),
            Value::Int4(0), // reltype
            Value::Int4(0), // reloftype
            Value::Int4(self.relation_owner_oid(owner) as i32),
            Value::Int4(relam),
            Value::Int4(0),      // relpages
            Value::Float4(-1.0), // reltuples
            Value::Int4(0),      // relallvisible
            Value::Int4(0),      // reltoastrelid
            Value::Bool(has_index),
            Value::Bool(false), // relisshared
            Value::Text("p".to_string()),
            Value::Text(kind.to_string()),
            Value::Int4(natts as i32),
            Value::Int4(0), // relchecks
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false), // relhassubclass
            Value::Bool(false), // relrowsecurity
            Value::Bool(false), // relforcerowsecurity
            Value::Bool(true),  // relispopulated
            Value::Text("d".to_string()),
            Value::Bool(false), // relispartition
            Value::Int4(0),     // relrewrite
            Value::Int4(0),     // relfrozenxid
            Value::Int4(0),     // relminmxid
            Value::Null,        // relacl
            Value::Null,        // reloptions
            Value::Null,        // relpartbound
            Value::Int4(0),     // reltablespace
        ]
    }
}

impl std::fmt::Debug for CatalogSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogSnapshot")
            .field("database", &self.database.name())
            .field("databases", &self.databases.len())
            .field("roles", &self.roles.len())
            .field("settings", &self.settings.len())
            .finish()
    }
}

/// Which system relation a [`crate::plan::Plan::CatalogScan`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogSource {
    /// `pg_database`
    Database,
    /// `pg_roles`
    Roles,
    /// `pg_user`
    User,
    /// `pg_namespace`
    Namespace,
    /// `pg_class`
    Class,
    /// `pg_attribute`
    Attribute,
    /// `pg_attrdef`
    AttrDef,
    /// `pg_type`
    Type,
    /// `pg_settings`
    Settings,
    /// `pg_tables`
    Tables,
    /// `pg_views`
    Views,
    /// `pg_indexes`
    Indexes,
    /// `pg_tablespace`
    Tablespace,
    /// `pg_constraint`
    Constraint,
    /// `pg_am`
    Am,
    /// `pg_index`
    Index,
    /// `pg_sequences` (the user-facing view)
    Sequences,
    /// `pg_sequence` (the backend catalog)
    Sequence,
    /// `information_schema.schemata`
    Schemata,
    /// `information_schema.tables`
    IsTables,
    /// `information_schema.columns`
    IsColumns,
    /// `information_schema.views`
    IsViews,
    /// `information_schema.table_constraints`
    IsTableConstraints,
    /// `information_schema.key_column_usage`
    IsKeyColumnUsage,
    /// A relation the engine recognises but has no rows for yet; it exists so a
    /// tool that joins it receives an empty set instead of an error.
    Empty(&'static str),
}

/// Relations that are always empty: the engine has no such objects yet.
///
/// They are declared with PostgreSQL's real column lists so a driver that
/// introspects them sees the expected shape.
const EMPTY_RELATIONS: &[(&str, &[(&str, DataType)])] = &[
    (
        "pg_matviews",
        &[
            ("schemaname", DataType::Text),
            ("matviewname", DataType::Text),
            ("matviewowner", DataType::Text),
            // PostgreSQL's own definition of this view is
            // `... C.relhasindex AS hasindexes ... T.spcname AS tablespace ...`
            // between the owner and `ispopulated`; tools select them even when
            // the relation is empty, so they must be present in this order.
            ("tablespace", DataType::Text),
            ("hasindexes", DataType::Bool),
            ("ispopulated", DataType::Bool),
            ("definition", DataType::Text),
        ],
    ),
    (
        "pg_extension",
        &[
            ("oid", DataType::Int4),
            ("extname", DataType::Text),
            ("extowner", DataType::Int4),
            ("extnamespace", DataType::Int4),
            ("extrelocatable", DataType::Bool),
            ("extversion", DataType::Text),
            ("extconfig", DataType::Text),
            ("extcondition", DataType::Text),
        ],
    ),
    (
        "pg_description",
        &[
            ("objoid", DataType::Int4),
            ("classoid", DataType::Int4),
            ("objsubid", DataType::Int4),
            ("description", DataType::Text),
        ],
    ),
    (
        "pg_depend",
        &[
            ("classid", DataType::Int4),
            ("objid", DataType::Int4),
            ("objsubid", DataType::Int4),
            ("refclassid", DataType::Int4),
            ("refobjid", DataType::Int4),
            ("refobjsubid", DataType::Int4),
            ("deptype", DataType::Text),
        ],
    ),
    (
        "pg_inherits",
        &[
            ("inhrelid", DataType::Int4),
            ("inhparent", DataType::Int4),
            ("inhseqno", DataType::Int4),
            ("inhdetachpending", DataType::Bool),
        ],
    ),
    (
        "pg_enum",
        &[
            ("oid", DataType::Int4),
            ("enumtypid", DataType::Int4),
            ("enumsortorder", DataType::Float4),
            ("enumlabel", DataType::Text),
        ],
    ),
    (
        "pg_trigger",
        &[
            ("oid", DataType::Int4),
            ("tgrelid", DataType::Int4),
            ("tgname", DataType::Text),
            ("tgfoid", DataType::Int4),
            ("tgtype", DataType::Int4),
            ("tgenabled", DataType::Text),
            ("tgisinternal", DataType::Bool),
            // The constraint a trigger enforces, when it was built for one
            // (`CHECK`/foreign-key triggers); `0` for the non-trigger tables
            // this engine materialises, which have no such triggers.
            ("tgconstrrelid", DataType::Int4),
        ],
    ),
    (
        "pg_collation",
        &[
            ("oid", DataType::Int4),
            ("collname", DataType::Text),
            ("collnamespace", DataType::Int4),
            ("collowner", DataType::Int4),
            ("collprovider", DataType::Text),
            ("collisdeterministic", DataType::Bool),
            ("collencoding", DataType::Int4),
            ("collctype", DataType::Text),
            ("colllocale", DataType::Text),
        ],
    ),
    (
        "pg_proc",
        &[
            ("oid", DataType::Int4),
            ("proname", DataType::Text),
            ("pronamespace", DataType::Int4),
            ("proowner", DataType::Int4),
            ("prokind", DataType::Text),
            ("proisagg", DataType::Bool),
            ("proiswindow", DataType::Bool),
            ("prorettype", DataType::Int4),
            ("proargtypes", DataType::Text),
            ("prosrc", DataType::Text),
        ],
    ),
    (
        "pg_foreign_table",
        &[
            ("ftrelid", DataType::Int4),
            ("ftserver", DataType::Int4),
            ("ftoptions", DataType::Text),
        ],
    ),
    (
        "pg_foreign_server",
        &[
            ("oid", DataType::Int4),
            ("srvname", DataType::Text),
            ("srvowner", DataType::Int4),
            ("srvfdw", DataType::Int4),
            ("srvtype", DataType::Text),
            ("srvversion", DataType::Text),
            ("srvacl", DataType::Text),
            ("svroptions", DataType::Text),
        ],
    ),
    (
        "pg_foreign_data_wrapper",
        &[
            ("oid", DataType::Int4),
            ("fdwname", DataType::Text),
            ("fdwowner", DataType::Int4),
            ("fdwhandler", DataType::Int4),
            ("fdwvalidator", DataType::Int4),
            ("fdwacl", DataType::Text),
            ("fdwoptions", DataType::Text),
        ],
    ),
    (
        "pg_user_mapping",
        &[
            ("oid", DataType::Int4),
            ("umuser", DataType::Int4),
            ("umserver", DataType::Int4),
            ("umoptions", DataType::Text),
        ],
    ),
    (
        "pg_partitioned_table",
        &[
            ("partrelid", DataType::Int4),
            ("partstrat", DataType::Text),
            ("partnatts", DataType::Int4),
            ("partdefid", DataType::Int4),
            ("partattrs", DataType::Text),
            ("partclass", DataType::Text),
            ("partcollation", DataType::Text),
            ("partbound", DataType::Text),
        ],
    ),
    (
        "pg_rewrite",
        &[
            ("oid", DataType::Int4),
            ("rulename", DataType::Text),
            ("ev_class", DataType::Int4),
            ("ev_type", DataType::Text),
            ("ev_enabled", DataType::Text),
            ("is_instead", DataType::Bool),
            ("ev_action", DataType::Text),
        ],
    ),
    (
        "pg_shdescription",
        &[
            ("objoid", DataType::Int4),
            ("classoid", DataType::Int4),
            ("description", DataType::Text),
        ],
    ),
    (
        "pg_aggregate",
        &[
            ("aggfnoid", DataType::Int4),
            ("aggkind", DataType::Text),
            ("aggnumdirectargs", DataType::Int4),
            ("aggtransfn", DataType::Int4),
            ("aggfinalfn", DataType::Int4),
            ("aggtranstype", DataType::Int4),
            ("agginitval", DataType::Text),
        ],
    ),
    (
        "pg_opclass",
        &[
            ("oid", DataType::Int4),
            ("opcmethod", DataType::Int4),
            ("opcname", DataType::Text),
            ("opcnamespace", DataType::Int4),
            ("opcowner", DataType::Int4),
            ("opcfamily", DataType::Int4),
            ("opcintype", DataType::Int4),
            ("opcdefault", DataType::Bool),
            ("opckeytype", DataType::Int4),
        ],
    ),
    (
        "pg_operator",
        &[
            ("oid", DataType::Int4),
            ("oprname", DataType::Text),
            ("oprnamespace", DataType::Int4),
            ("oprowner", DataType::Int4),
            ("oprkind", DataType::Text),
            ("oprcanmerge", DataType::Bool),
            ("oprcanhash", DataType::Bool),
            ("oprleft", DataType::Int4),
            ("oprright", DataType::Int4),
            ("oprcom", DataType::Int4),
            ("oprnegate", DataType::Int4),
            ("oprcode", DataType::Text),
        ],
    ),
    (
        "pg_statistic",
        &[
            ("starelid", DataType::Int4),
            ("staattnum", DataType::Int4),
            ("stainherit", DataType::Bool),
            ("stanullfrac", DataType::Float4),
            ("stawidth", DataType::Int4),
            ("stadistinct", DataType::Float4),
        ],
    ),
    (
        "pg_policy",
        &[
            ("oid", DataType::Int4),
            ("polname", DataType::Text),
            ("polrelid", DataType::Int4),
            ("polcmd", DataType::Text),
            ("polpermissive", DataType::Bool),
            ("polroles", DataType::Text),
            ("polqual", DataType::Int4),
            ("polwithcheck", DataType::Int4),
        ],
    ),
    (
        "pg_publication",
        &[
            ("oid", DataType::Int4),
            ("pubname", DataType::Text),
            ("pubowner", DataType::Int4),
            ("puballtables", DataType::Bool),
            ("pubinsert", DataType::Bool),
            ("pubupdate", DataType::Bool),
            ("pubdelete", DataType::Bool),
            ("pubtruncate", DataType::Bool),
            ("pubviaroot", DataType::Bool),
        ],
    ),
    (
        "referential_constraints",
        &[
            ("constraint_catalog", DataType::Text),
            ("constraint_schema", DataType::Text),
            ("constraint_name", DataType::Text),
            ("unique_constraint_catalog", DataType::Text),
            ("unique_constraint_schema", DataType::Text),
            ("unique_constraint_name", DataType::Text),
            ("match_option", DataType::Text),
            ("update_rule", DataType::Text),
            ("delete_rule", DataType::Text),
        ],
    ),
    (
        "check_constraints",
        &[
            ("constraint_catalog", DataType::Text),
            ("constraint_schema", DataType::Text),
            ("constraint_name", DataType::Text),
            ("check_clause", DataType::Text),
        ],
    ),
    (
        "routines",
        &[
            ("specific_catalog", DataType::Text),
            ("specific_schema", DataType::Text),
            ("specific_name", DataType::Text),
            ("routine_catalog", DataType::Text),
            ("routine_schema", DataType::Text),
            ("routine_name", DataType::Text),
            ("routine_type", DataType::Text),
        ],
    ),
    (
        "triggers",
        &[
            ("trigger_catalog", DataType::Text),
            ("trigger_schema", DataType::Text),
            ("trigger_name", DataType::Text),
            ("event_object_catalog", DataType::Text),
            ("event_object_schema", DataType::Text),
            ("event_object_table", DataType::Text),
            ("action_timing", DataType::Text),
        ],
    ),
    (
        "sequences",
        &[
            ("sequence_catalog", DataType::Text),
            ("sequence_schema", DataType::Text),
            ("sequence_name", DataType::Text),
            ("data_type", DataType::Text),
            ("numeric_precision", DataType::Int4),
            ("numeric_precision_radix", DataType::Int4),
            ("numeric_scale", DataType::Int4),
            ("start_value", DataType::Text),
            ("minimum_value", DataType::Text),
            ("maximum_value", DataType::Text),
            ("increment", DataType::Text),
            ("cycle_option", DataType::Text),
        ],
    ),
    (
        "parameters",
        &[
            ("specific_catalog", DataType::Text),
            ("specific_schema", DataType::Text),
            ("specific_name", DataType::Text),
            ("ordinal_position", DataType::Int4),
            ("parameter_mode", DataType::Text),
            ("is_result", DataType::Text),
            ("as_locator", DataType::Text),
            ("parameter_name", DataType::Text),
            ("data_type", DataType::Text),
            ("character_maximum_length", DataType::Int4),
            ("character_octet_length", DataType::Int4),
            ("character_set_catalog", DataType::Text),
            ("character_set_schema", DataType::Text),
            ("character_set_name", DataType::Text),
            ("collation_catalog", DataType::Text),
            ("collation_schema", DataType::Text),
            ("collation_name", DataType::Text),
            ("numeric_precision", DataType::Int4),
            ("numeric_precision_radix", DataType::Int4),
            ("numeric_scale", DataType::Int4),
            ("datetime_precision", DataType::Int4),
            ("interval_type", DataType::Text),
            ("interval_precision", DataType::Int4),
            ("udt_catalog", DataType::Text),
            ("udt_schema", DataType::Text),
            ("udt_name", DataType::Text),
            ("scope_catalog", DataType::Text),
            ("scope_schema", DataType::Text),
            ("scope_name", DataType::Text),
            ("maximum_cardinality", DataType::Int4),
            ("dtd_identifier", DataType::Text),
            ("parameter_default", DataType::Text),
        ],
    ),
    (
        "pg_subscription",
        &[
            ("oid", DataType::Int4),
            ("subdbid", DataType::Int4),
            ("subname", DataType::Text),
            ("subowner", DataType::Int4),
            ("subenabled", DataType::Bool),
            ("subconninfo", DataType::Text),
            ("subslotname", DataType::Text),
            ("subpublications", DataType::Text),
        ],
    ),
];

impl CatalogSource {
    /// Resolves a (possibly qualified) relation name to a system relation.
    ///
    /// `pg_catalog`-qualified names must be one of the relations above. An
    /// *unqualified* name only reaches the catalogue when it starts with `pg_`,
    /// which user objects can never be called (`pg_` is reserved), so the
    /// catalogue effectively shadows nothing a user could create - the same
    /// precedence `pg_catalog` has at the front of PostgreSQL's search path.
    ///
    /// A database-qualified name (`db.schema.relation`, as some clients emit)
    /// resolves on its last two components, matching how a user table of the
    /// same shape is resolved.
    pub fn resolve(name: &[String]) -> Option<CatalogSource> {
        let (schema, relation) = match name {
            [relation] => (None, relation),
            [.., schema, relation] => (Some(schema), relation),
            [] => return None,
        };
        let folded = relation.to_ascii_lowercase();
        match schema.map(|schema| schema.to_ascii_lowercase()) {
            // Unqualified names reach `pg_catalog` only when they look like
            // system relations; `pg_` is reserved, so nothing is shadowed.
            None if relation.starts_with("pg_") => pg_catalog_source(&folded),
            Some(schema) if schema == "pg_catalog" => pg_catalog_source(&folded),
            // `information_schema` relations are qualified-only: user tables
            // named `columns` or `tables` must keep resolving in `public`.
            Some(schema) if schema == "information_schema" => information_schema_source(&folded),
            _ => None,
        }
    }

    /// The relation's schema, column for column with PostgreSQL's.
    pub fn schema(self) -> Schema {
        let columns: &[(&str, DataType)] = match self {
            CatalogSource::Database => &[
                ("oid", DataType::Int4),
                ("datname", DataType::Text),
                ("datdba", DataType::Int4),
                ("encoding", DataType::Int4),
                ("datistemplate", DataType::Bool),
                ("datallowconn", DataType::Bool),
                ("datconnlimit", DataType::Int4),
                ("datfrozenxid", DataType::Int4),
                ("datminmxid", DataType::Int4),
                ("dattablespace", DataType::Int4),
                ("datcollate", DataType::Text),
                ("datctype", DataType::Text),
                ("datlocprovider", DataType::Text),
                ("daticulocale", DataType::Text),
                ("daticurules", DataType::Text),
                ("datcollversion", DataType::Text),
                ("datacl", DataType::Text),
            ],
            CatalogSource::Roles => &[
                ("rolname", DataType::Text),
                ("rolsuper", DataType::Bool),
                ("rolinherit", DataType::Bool),
                ("rolcreaterole", DataType::Bool),
                ("rolcreatedb", DataType::Bool),
                ("rolcanlogin", DataType::Bool),
                ("rolreplication", DataType::Bool),
                ("rolconnlimit", DataType::Int4),
                ("rolpassword", DataType::Text),
                ("rolvaliduntil", DataType::Text),
                ("rolbypassrls", DataType::Bool),
                ("rolconfig", DataType::Text),
                ("oid", DataType::Int4),
            ],
            CatalogSource::User => &[
                ("usename", DataType::Text),
                ("usesuper", DataType::Bool),
                ("usecreatedb", DataType::Bool),
                ("userepl", DataType::Bool),
                ("usebypassrls", DataType::Bool),
                ("passwd", DataType::Text),
                ("valuntil", DataType::Text),
                ("useconfig", DataType::Text),
            ],
            CatalogSource::Namespace => &[
                ("oid", DataType::Int4),
                ("nspname", DataType::Text),
                ("nspowner", DataType::Int4),
                ("nspacl", DataType::Text),
            ],
            CatalogSource::Class => &[
                ("oid", DataType::Int4),
                ("relname", DataType::Text),
                ("relnamespace", DataType::Int4),
                ("reltype", DataType::Int4),
                ("reloftype", DataType::Int4),
                ("relowner", DataType::Int4),
                ("relam", DataType::Int4),
                ("relpages", DataType::Int4),
                ("reltuples", DataType::Float4),
                ("relallvisible", DataType::Int4),
                ("reltoastrelid", DataType::Int4),
                ("relhasindex", DataType::Bool),
                ("relisshared", DataType::Bool),
                ("relpersistence", DataType::Text),
                ("relkind", DataType::Text),
                ("relnatts", DataType::Int4),
                ("relchecks", DataType::Int4),
                ("relhasrules", DataType::Bool),
                ("relhastriggers", DataType::Bool),
                ("relhassubclass", DataType::Bool),
                ("relrowsecurity", DataType::Bool),
                ("relforcerowsecurity", DataType::Bool),
                ("relispopulated", DataType::Bool),
                ("relreplident", DataType::Text),
                ("relispartition", DataType::Bool),
                ("relrewrite", DataType::Int4),
                ("relfrozenxid", DataType::Int4),
                ("relminmxid", DataType::Int4),
                ("relacl", DataType::Text),
                ("reloptions", DataType::Text),
                ("relpartbound", DataType::Text),
                ("reltablespace", DataType::Int4),
            ],
            CatalogSource::Attribute => &[
                ("attrelid", DataType::Int4),
                ("attname", DataType::Text),
                ("atttypid", DataType::Int4),
                ("attlen", DataType::Int4),
                ("attnum", DataType::Int4),
                ("attcacheoff", DataType::Int4),
                ("atttypmod", DataType::Int4),
                ("attndims", DataType::Int4),
                ("attbyval", DataType::Bool),
                ("attalign", DataType::Text),
                ("attstorage", DataType::Text),
                ("attcompression", DataType::Text),
                ("attnotnull", DataType::Bool),
                ("atthasdef", DataType::Bool),
                ("atthasmissing", DataType::Bool),
                ("attidentity", DataType::Text),
                ("attgenerated", DataType::Text),
                ("attisdropped", DataType::Bool),
                ("attislocal", DataType::Bool),
                ("attinhcount", DataType::Int4),
                ("attcollation", DataType::Int4),
                ("attstattarget", DataType::Int4),
                ("attacl", DataType::Text),
                ("attoptions", DataType::Text),
                ("attfdwoptions", DataType::Text),
                ("attmissingval", DataType::Text),
            ],
            CatalogSource::AttrDef => &[
                // PostgreSQL 15's column list (`adsrc` was removed in 12).
                ("oid", DataType::Int4),
                ("adrelid", DataType::Int4),
                ("adnum", DataType::Int4),
                ("adbin", DataType::Text),
            ],
            CatalogSource::Sequences => &[
                // PostgreSQL 13's `pg_sequences` view.
                ("schemaname", DataType::Text),
                ("sequencename", DataType::Text),
                ("sequenceowner", DataType::Text),
                ("data_type", DataType::Text),
                ("start_value", DataType::Int8),
                ("min_value", DataType::Int8),
                ("max_value", DataType::Int8),
                ("increment_by", DataType::Int8),
                ("cycle", DataType::Bool),
                ("cache_size", DataType::Int8),
                ("last_value", DataType::Int8),
            ],
            CatalogSource::Sequence => &[
                // PostgreSQL 10's backend catalog.
                ("seqrelid", DataType::Int4),
                ("seqtypid", DataType::Int4),
                ("seqstart", DataType::Int8),
                ("seqincrement", DataType::Int8),
                ("seqmax", DataType::Int8),
                ("seqmin", DataType::Int8),
                ("seqcache", DataType::Int8),
                ("seqcycle", DataType::Bool),
            ],
            CatalogSource::Type => &[
                ("oid", DataType::Int4),
                ("typname", DataType::Text),
                ("typnamespace", DataType::Int4),
                ("typlen", DataType::Int4),
                ("typbyval", DataType::Bool),
                ("typtype", DataType::Text),
                ("typcategory", DataType::Text),
                ("typdelim", DataType::Text),
                ("typisdefined", DataType::Bool),
                ("typarray", DataType::Int4),
                ("typinput", DataType::Text),
                ("typoutput", DataType::Text),
                // The remaining columns browsers join on - `typelem` to name the
                // element type of an array, `typrelid` to find a composite's
                // relation, `typdefault`/`typnotnull` for domains. PostgreSQL
                // orders them differently; they are appended here so the column
                // positions clients have already cached keep their meaning.
                ("typelem", DataType::Int4),
                ("typrelid", DataType::Int4),
                ("typnotnull", DataType::Bool),
                ("typbasetype", DataType::Int4),
                ("typcollation", DataType::Int4),
                ("typndims", DataType::Int4),
                ("typdefault", DataType::Text),
                ("typdefaultbin", DataType::Text),
                ("typacl", DataType::Text),
                ("typreceive", DataType::Text),
                ("typsend", DataType::Text),
                ("typalign", DataType::Text),
                ("typstorage", DataType::Text),
                ("typispreferred", DataType::Bool),
                ("typowner", DataType::Int4),
            ],
            CatalogSource::Settings => &[
                ("name", DataType::Text),
                ("setting", DataType::Text),
                ("unit", DataType::Text),
                ("category", DataType::Text),
                ("short_desc", DataType::Text),
                ("extra_desc", DataType::Text),
                ("context", DataType::Text),
                ("vartype", DataType::Text),
                ("source", DataType::Text),
                ("min_val", DataType::Text),
                ("max_val", DataType::Text),
                ("enumvals", DataType::Text),
                ("boot_val", DataType::Text),
                ("reset_val", DataType::Text),
                ("pending_restart", DataType::Bool),
            ],
            CatalogSource::Tables => &[
                ("schemaname", DataType::Text),
                ("tablename", DataType::Text),
                ("tableowner", DataType::Text),
                // The tablespace the table lives in; the engine has no custom
                // tablespaces, which PostgreSQL reports as NULL.
                ("tablespace", DataType::Text),
                ("hasindexes", DataType::Bool),
                ("hasrules", DataType::Bool),
                ("hastriggers", DataType::Bool),
                ("rowsecurity", DataType::Bool),
            ],
            CatalogSource::Views => &[
                ("schemaname", DataType::Text),
                ("viewname", DataType::Text),
                ("viewowner", DataType::Text),
                ("definition", DataType::Text),
            ],
            CatalogSource::Indexes => &[
                ("schemaname", DataType::Text),
                ("tablename", DataType::Text),
                ("indexname", DataType::Text),
                ("tablespace", DataType::Text),
                ("indexdef", DataType::Text),
            ],
            CatalogSource::Tablespace => &[
                ("oid", DataType::Int4),
                ("spcname", DataType::Text),
                ("spcowner", DataType::Int4),
                ("spcacl", DataType::Text),
                ("spcoptions", DataType::Text),
            ],
            CatalogSource::Constraint => &[
                ("oid", DataType::Int4),
                ("conname", DataType::Text),
                ("connamespace", DataType::Int4),
                ("contype", DataType::Text),
                ("conrelid", DataType::Int4),
                ("conindid", DataType::Int4),
                ("confrelid", DataType::Int4),
                ("conkey", DataType::Text),
                ("confkey", DataType::Text),
                ("convalidated", DataType::Bool),
                ("condeferrable", DataType::Bool),
                ("condeferred", DataType::Bool),
                ("conparentid", DataType::Int4),
                ("confupdtype", DataType::Text),
                ("confdeltype", DataType::Text),
                ("confmatchtype", DataType::Text),
                ("conpfeqop", DataType::Text),
                ("conppeqop", DataType::Text),
                ("conffeqop", DataType::Text),
                ("coninhcount", DataType::Int4),
                ("connoinherit", DataType::Bool),
                ("conbin", DataType::Text),
                // Exclusion-operator array, non-NULL only for exclusion
                // constraints; every constraint this engine reports is
                // primary-key/unique/foreign-key/check, so it stays NULL.
                ("conexclop", DataType::Text),
            ],
            CatalogSource::Am => &[
                ("oid", DataType::Int4),
                ("amname", DataType::Text),
                ("amhandler", DataType::Text),
                ("amtype", DataType::Text),
            ],
            CatalogSource::Schemata => &[
                ("catalog_name", DataType::Text),
                ("schema_name", DataType::Text),
                ("schema_owner", DataType::Text),
                ("default_character_set_catalog", DataType::Text),
                ("default_character_set_schema", DataType::Text),
                ("sql_path", DataType::Text),
            ],
            CatalogSource::IsTables => &[
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("table_type", DataType::Text),
                ("self_referencing_column_name", DataType::Text),
                ("reference_generation", DataType::Text),
                ("user_defined_type_catalog", DataType::Text),
                ("user_defined_type_schema", DataType::Text),
                ("user_defined_type_name", DataType::Text),
                ("is_insertable_into", DataType::Text),
                ("is_typed", DataType::Text),
                ("commit_action", DataType::Text),
            ],
            CatalogSource::IsColumns => &[
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("column_name", DataType::Text),
                ("ordinal_position", DataType::Int4),
                ("column_default", DataType::Text),
                ("is_nullable", DataType::Text),
                ("data_type", DataType::Text),
                ("character_maximum_length", DataType::Int4),
                ("character_octet_length", DataType::Int4),
                ("numeric_precision", DataType::Int4),
                ("numeric_precision_radix", DataType::Int4),
                ("numeric_scale", DataType::Int4),
                ("datetime_precision", DataType::Int4),
                ("interval_type", DataType::Text),
                ("interval_precision", DataType::Int4),
                ("character_set_catalog", DataType::Text),
                ("character_set_schema", DataType::Text),
                ("character_set_name", DataType::Text),
                ("collation_catalog", DataType::Text),
                ("collation_schema", DataType::Text),
                ("collation_name", DataType::Text),
                ("domain_catalog", DataType::Text),
                ("domain_schema", DataType::Text),
                ("domain_name", DataType::Text),
                ("udt_catalog", DataType::Text),
                ("udt_schema", DataType::Text),
                ("udt_name", DataType::Text),
                ("scope_catalog", DataType::Text),
                ("scope_schema", DataType::Text),
                ("scope_name", DataType::Text),
                ("maximum_cardinality", DataType::Int4),
                ("dtd_identifier", DataType::Text),
                ("is_self_referencing", DataType::Text),
                ("is_identity", DataType::Text),
                ("identity_generation", DataType::Text),
                ("identity_start", DataType::Text),
                ("identity_increment", DataType::Text),
                ("identity_maximum", DataType::Text),
                ("identity_minimum", DataType::Text),
                ("identity_cycle", DataType::Text),
                ("is_generated", DataType::Text),
                ("generation_expression", DataType::Text),
                ("is_updatable", DataType::Text),
            ],
            CatalogSource::IsViews => &[
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("view_definition", DataType::Text),
                ("check_option", DataType::Text),
                ("is_updatable", DataType::Text),
                ("is_insertable_into", DataType::Text),
                ("is_trigger_updatable", DataType::Text),
                ("is_trigger_deletable", DataType::Text),
                ("is_trigger_insertable_into", DataType::Text),
            ],
            CatalogSource::IsTableConstraints => &[
                ("constraint_catalog", DataType::Text),
                ("constraint_schema", DataType::Text),
                ("constraint_name", DataType::Text),
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("constraint_type", DataType::Text),
                ("is_deferrable", DataType::Text),
                ("initially_deferred", DataType::Text),
                ("enforced", DataType::Text),
            ],
            CatalogSource::IsKeyColumnUsage => &[
                ("constraint_catalog", DataType::Text),
                ("constraint_schema", DataType::Text),
                ("constraint_name", DataType::Text),
                ("table_catalog", DataType::Text),
                ("table_schema", DataType::Text),
                ("table_name", DataType::Text),
                ("column_name", DataType::Text),
                ("ordinal_position", DataType::Int4),
                ("position_in_unique_constraint", DataType::Int4),
            ],
            CatalogSource::Index => &[
                ("indexrelid", DataType::Int4),
                ("indrelid", DataType::Int4),
                ("indnatts", DataType::Int4),
                ("indnkeyatts", DataType::Int4),
                ("indisunique", DataType::Bool),
                ("indisnullsnotdistinct", DataType::Bool),
                ("indisprimary", DataType::Bool),
                ("indisexclusion", DataType::Bool),
                ("indimmediate", DataType::Bool),
                ("indisclustered", DataType::Bool),
                ("indisvalid", DataType::Bool),
                ("indcheckxmin", DataType::Bool),
                ("indisready", DataType::Bool),
                ("indislive", DataType::Bool),
                ("indisreplident", DataType::Bool),
                ("indkey", DataType::Text),
                ("indcollation", DataType::Text),
                ("indclass", DataType::Text),
                ("indoption", DataType::Text),
                ("indexprs", DataType::Text),
                ("indpred", DataType::Text),
            ],
            CatalogSource::Empty(name) => {
                let columns = EMPTY_RELATIONS
                    .iter()
                    .find(|(known, _)| *known == name)
                    .map(|(_, columns)| *columns)
                    .unwrap_or(&[]);
                return columns
                    .iter()
                    .map(|(column, data_type)| Field::unqualified(*column, data_type.clone()))
                    .collect();
            }
        };
        let mut fields: Vec<Field> = columns
            .iter()
            .map(|(name, data_type)| Field::unqualified(*name, data_type.clone()))
            .collect();
        // `tableoid`: the hidden system column every PostgreSQL row carries.
        // Clients filter on it to tell which catalogue a row came from.
        fields.push(Field::unqualified("tableoid", DataType::Int4));
        fields
    }

    /// The name `EXPLAIN` prints.
    pub fn name(self) -> &'static str {
        match self {
            CatalogSource::Database => "pg_database",
            CatalogSource::Roles => "pg_roles",
            CatalogSource::User => "pg_user",
            CatalogSource::Namespace => "pg_namespace",
            CatalogSource::Class => "pg_class",
            CatalogSource::Attribute => "pg_attribute",
            CatalogSource::AttrDef => "pg_attrdef",
            CatalogSource::Type => "pg_type",
            CatalogSource::Settings => "pg_settings",
            CatalogSource::Tables => "pg_tables",
            CatalogSource::Views => "pg_views",
            CatalogSource::Indexes => "pg_indexes",
            CatalogSource::Tablespace => "pg_tablespace",
            CatalogSource::Constraint => "pg_constraint",
            CatalogSource::Am => "pg_am",
            CatalogSource::Index => "pg_index",
            CatalogSource::Sequences => "pg_sequences",
            CatalogSource::Sequence => "pg_sequence",
            CatalogSource::Schemata => "information_schema.schemata",
            CatalogSource::IsTables => "information_schema.tables",
            CatalogSource::IsColumns => "information_schema.columns",
            CatalogSource::IsViews => "information_schema.views",
            CatalogSource::IsTableConstraints => "information_schema.table_constraints",
            CatalogSource::IsKeyColumnUsage => "information_schema.key_column_usage",
            CatalogSource::Empty(name) => name,
        }
    }
}

/// `pg_type.typlen`: the fixed width, or `-1` for variable-length types.
///
/// Mirrors the wire encoder's table so `pg_attribute.attlen` agrees with what a
/// `RowDescription` for the same column reports.
fn typlen(data_type: &DataType) -> i32 {
    match data_type {
        DataType::Bool => 1,
        DataType::Int2 => 2,
        DataType::Int4 | DataType::Date | DataType::Float4 => 4,
        DataType::Int8
        | DataType::Float8
        | DataType::Time
        | DataType::Timestamp
        | DataType::Timestamptz => 8,
        DataType::Interval | DataType::Uuid => 16,
        DataType::Numeric { .. }
        | DataType::Text
        | DataType::Varchar(_)
        | DataType::Char(_)
        | DataType::Bytea
        | DataType::Json
        | DataType::Jsonb
        | DataType::Array(_) => -1,
    }
}

/// One `pg_type` row, for a base type or for the array type built over one.
///
/// The column order matches `CatalogSource::Type::fields`; the two are changed
/// together.
fn type_row(name: &str, data_type: &DataType, category: &str, owner: i32) -> Vec<Value> {
    let element = typelem(data_type);
    vec![
        Value::Int4(data_type.oid() as i32),
        Value::Text(name.to_string()),
        Value::Int4(PG_CATALOG_NAMESPACE),
        Value::Int4(typlen(data_type)),
        Value::Bool(matches!(
            data_type,
            DataType::Bool | DataType::Int2 | DataType::Int4 | DataType::Float4
        )),
        Value::Text("b".to_string()),
        Value::Text(category.to_string()),
        Value::Text(",".to_string()),
        Value::Bool(true),
        Value::Int4(data_type.array_oid() as i32),
        Value::Text("-".to_string()),
        Value::Text("-".to_string()),
        Value::Int4(element),
        Value::Int4(0),     // typrelid: no composite types yet
        Value::Bool(false), // typnotnull: no domains
        Value::Int4(0),     // typbasetype: not a domain
        Value::Int4(0),     // typcollation: the default collation
        // PostgreSQL reports one dimension for an array type.
        Value::Int4(if element == 0 { 0 } else { 1 }),
        Value::Null, // typdefault
        Value::Null, // typdefaultbin
        Value::Null, // typacl
        Value::Text("-".to_string()),
        Value::Text("-".to_string()),
        Value::Text(typalign(data_type).to_string()),
        Value::Text(typstorage(data_type).to_string()),
        Value::Bool(false), // typispreferred
        Value::Int4(owner),
    ]
}

/// `pg_type.typelem`: the element type of an array, `0` for everything else.
fn typelem(data_type: &DataType) -> i32 {
    match data_type {
        DataType::Array(element) => element.oid() as i32,
        _ => 0,
    }
}

/// `pg_type.typalign`: PostgreSQL's alignment code - `c`har, `s`hort, `i`nt or
/// `d`ouble - saying where inside a tuple a value of this type may start.
fn typalign(data_type: &DataType) -> char {
    match data_type {
        DataType::Bool | DataType::Char(_) => 'c',
        DataType::Int2 => 's',
        DataType::Int4 | DataType::Float4 | DataType::Date => 'i',
        DataType::Int8
        | DataType::Float8
        | DataType::Time
        | DataType::Timestamp
        | DataType::Timestamptz => 'd',
        _ => 'i',
    }
}

/// `pg_type.typstorage`: `p`lain for fixed-width types, e`x`tended for the
/// varlena types, which PostgreSQL also keeps short values in-line for.
fn typstorage(data_type: &DataType) -> char {
    match data_type {
        DataType::Bool
        | DataType::Int2
        | DataType::Int4
        | DataType::Int8
        | DataType::Float4
        | DataType::Float8
        | DataType::Date
        | DataType::Time
        | DataType::Timestamp
        | DataType::Timestamptz => 'p',
        _ => 'x',
    }
}

/// Which `pg_catalog` relation `name` (already folded) refers to.
fn pg_catalog_source(name: &str) -> Option<CatalogSource> {
    match name {
        "pg_database" => Some(CatalogSource::Database),
        "pg_roles" => Some(CatalogSource::Roles),
        "pg_user" => Some(CatalogSource::User),
        "pg_namespace" => Some(CatalogSource::Namespace),
        "pg_class" => Some(CatalogSource::Class),
        "pg_attribute" => Some(CatalogSource::Attribute),
        "pg_attrdef" => Some(CatalogSource::AttrDef),
        "pg_type" => Some(CatalogSource::Type),
        "pg_settings" => Some(CatalogSource::Settings),
        "pg_tables" => Some(CatalogSource::Tables),
        "pg_views" => Some(CatalogSource::Views),
        "pg_indexes" => Some(CatalogSource::Indexes),
        "pg_tablespace" => Some(CatalogSource::Tablespace),
        "pg_constraint" => Some(CatalogSource::Constraint),
        "pg_am" => Some(CatalogSource::Am),
        "pg_index" => Some(CatalogSource::Index),
        "pg_sequences" => Some(CatalogSource::Sequences),
        "pg_sequence" => Some(CatalogSource::Sequence),
        other => EMPTY_RELATIONS
            .iter()
            .find(|(known, _)| *known == other)
            .map(|(known, _)| CatalogSource::Empty(known)),
    }
}

/// Which `information_schema` relation `name` (already folded) refers to.
fn information_schema_source(name: &str) -> Option<CatalogSource> {
    match name {
        "schemata" => Some(CatalogSource::Schemata),
        "tables" => Some(CatalogSource::IsTables),
        "columns" => Some(CatalogSource::IsColumns),
        "views" => Some(CatalogSource::IsViews),
        "table_constraints" => Some(CatalogSource::IsTableConstraints),
        "key_column_usage" => Some(CatalogSource::IsKeyColumnUsage),
        other => EMPTY_RELATIONS
            .iter()
            .find(|(known, _)| *known == other)
            .map(|(known, _)| CatalogSource::Empty(known)),
    }
}

/// The built-in types `pg_type` reports, as PostgreSQL names them.
const BUILTIN_TYPES: &[(&str, DataType, &str)] = &[
    ("bool", DataType::Bool, "B"),
    ("bytea", DataType::Bytea, "U"),
    ("char", DataType::Char(Some(1)), "S"),
    ("int8", DataType::Int8, "N"),
    ("int2", DataType::Int2, "N"),
    ("int4", DataType::Int4, "N"),
    ("text", DataType::Text, "S"),
    ("float4", DataType::Float4, "N"),
    ("float8", DataType::Float8, "N"),
    (
        "numeric",
        DataType::Numeric {
            precision: None,
            scale: None,
        },
        "N",
    ),
    ("varchar", DataType::Varchar(None), "S"),
    ("uuid", DataType::Uuid, "U"),
    ("date", DataType::Date, "D"),
    ("time", DataType::Time, "D"),
    ("timestamp", DataType::Timestamp, "D"),
    ("timestamptz", DataType::Timestamptz, "D"),
    ("interval", DataType::Interval, "T"),
    ("json", DataType::Json, "U"),
    ("jsonb", DataType::Jsonb, "U"),
];

/// The standard data type name `information_schema.columns.data_type` reports.
fn standard_data_type(data_type: &DataType) -> String {
    let name = match data_type {
        DataType::Bool => "boolean",
        DataType::Int2 => "smallint",
        DataType::Int4 => "integer",
        DataType::Int8 => "bigint",
        DataType::Float4 => "real",
        DataType::Float8 => "double precision",
        DataType::Numeric { .. } => "numeric",
        DataType::Text => "text",
        DataType::Varchar(_) => "character varying",
        DataType::Char(_) => "character",
        DataType::Bytea => "bytea",
        DataType::Date => "date",
        DataType::Time => "time without time zone",
        DataType::Timestamp => "timestamp without time zone",
        DataType::Timestamptz => "timestamp with time zone",
        DataType::Interval => "interval",
        DataType::Uuid => "uuid",
        DataType::Json => "json",
        DataType::Jsonb => "jsonb",
        DataType::Array(_) => "ARRAY",
    };
    name.to_string()
}

/// The internal type name `information_schema.columns.udt_name` reports.
fn pg_internal_type_name(data_type: &DataType) -> String {
    let name = match data_type {
        DataType::Bool => "bool",
        DataType::Int2 => "int2",
        DataType::Int4 => "int4",
        DataType::Int8 => "int8",
        DataType::Float4 => "float4",
        DataType::Float8 => "float8",
        DataType::Numeric { .. } => "numeric",
        DataType::Text => "text",
        DataType::Varchar(_) => "varchar",
        DataType::Char(_) => "bpchar",
        DataType::Bytea => "bytea",
        DataType::Date => "date",
        DataType::Time => "time",
        DataType::Timestamp => "timestamp",
        DataType::Timestamptz => "timestamptz",
        DataType::Interval => "interval",
        DataType::Uuid => "uuid",
        DataType::Json => "json",
        DataType::Jsonb => "jsonb",
        DataType::Array(_) => "array",
    };
    name.to_string()
}

/// `pg_attribute.atttypmod`, PostgreSQL's encoding of length parameters.
///
/// `varchar(n)`/`char(n)` store `n + 4`; `numeric(p, s)` stores
/// `((p << 16) | s) + 4`; everything else reports `-1`. `format_type` decodes
/// the same encoding, so a tool round-tripping `atttypid` + `atttypmod`
/// through it sees the declared length and precision.
fn atttypmod(data_type: &DataType) -> i32 {
    match data_type {
        DataType::Varchar(Some(n)) | DataType::Char(Some(n)) => *n as i32 + 4,
        DataType::Numeric {
            precision: Some(p),
            scale,
        } => {
            let scale = scale.map(|scale| scale as i32).unwrap_or(0);
            (((*p as i32) << 16) | scale) + 4
        }
        _ => -1,
    }
}

/// Renders a `CREATE INDEX` statement, as `pg_indexes.indexdef` and
/// `pg_get_indexdef` report it.
pub(crate) fn index_def_sql(table: &TableDef, index: &IndexDef) -> String {
    let columns = index
        .columns
        .iter()
        .map(|&ordinal| {
            table
                .columns
                .iter()
                .find(|column| column.ordinal == ordinal)
                .map(|column| column.name.clone())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE {}INDEX {} ON {} USING {} ({columns})",
        if index.unique { "UNIQUE " } else { "" },
        index.name,
        table.name,
        match index.method {
            IndexMethod::BTree => "btree",
            IndexMethod::Hash => "hash",
        }
    )
}

/// Renders a constraint's definition, as `pg_get_constraintdef` reports it.
///
/// The enumeration mirrors the `pg_constraint` scan — a table's primary key and
/// unique indexes first, then its foreign keys and check constraints — so the
/// oid a browser reads from `pg_constraint.oid` resolves to the same constraint
/// here. PostgreSQL renders `FOREIGN KEY ... REFERENCES ...` with the referenced
/// table's name, which the engine resolves from the stored relation oid.
pub(crate) fn constraint_def_sql(catalog: &Catalog, oid: Oid) -> Option<String> {
    let column_names = |table: &TableDef, ordinals: &[usize]| -> String {
        ordinals
            .iter()
            .filter_map(|&ordinal| {
                table
                    .columns
                    .iter()
                    .find(|column| column.ordinal == ordinal)
                    .map(|column| column.name.clone())
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    for table in catalog.tables() {
        let indexes = catalog.table_indexes(table.oid);
        if !table.primary_key.is_empty() {
            let is_primary = indexes
                .iter()
                .find(|index| index.primary)
                .is_some_and(|index| index.oid == oid);
            if is_primary {
                return Some(format!(
                    "PRIMARY KEY ({})",
                    column_names(&table, &table.primary_key)
                ));
            }
        }
        for index in indexes
            .iter()
            .filter(|index| index.unique && !index.primary)
        {
            if index.oid == oid {
                return Some(format!("UNIQUE ({})", column_names(&table, &index.columns)));
            }
        }
        for key in &table.foreign_keys {
            if key.oid == oid {
                let referenced = catalog
                    .table(key.ref_table)
                    .map(|def| def.name.clone())
                    .unwrap_or_default();
                return Some(format!(
                    "FOREIGN KEY ({}) REFERENCES {}({})",
                    column_names(&table, &key.columns),
                    referenced,
                    column_names(&table, &key.ref_columns),
                ));
            }
        }
        for check in &table.checks {
            if check.oid == oid {
                return Some(format!("CHECK ({})", check.expr));
            }
        }
    }
    None
}

/// A deterministic OID for a view, which has no storage of its own.
///
/// Hashed into a range far above the user OID watermark; stable for a given
/// `(schema, name)`, and never the OID of real storage.
pub(crate) fn view_oid(schema_oid: Oid, name: &str) -> Oid {
    let mut hash = 0x811c_9dc5;
    for byte in schema_oid.to_be_bytes().iter().chain(name.as_bytes()) {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    1_000_000 + hash % 1_000_000
}

/// A deterministic OID for a `pg_attrdef` row, which PostgreSQL allocates from
/// the shared OID counter.
///
/// Hashed into the reserved range above the view range, so it can never collide
/// with a real object OID nor with a view's synthetic OID.
fn attrdef_oid(table_oid: Oid, adnum: i32) -> Oid {
    let mut hash = 0x811c_9dc5;
    for byte in table_oid
        .to_be_bytes()
        .iter()
        .chain(adnum.to_be_bytes().iter())
    {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    2_000_000 + hash % 1_000_000
}

/// `n` zero positions, the text form of an all-zero `oidvector`.
fn zero_vector(count: usize) -> String {
    vec!["0"; count].join(" ")
}

/// Renders zero-based column ordinals as a one-based array literal.
fn key_literal(columns: &[usize]) -> String {
    columns
        .iter()
        .map(|&ordinal| (ordinal + 1).to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The relation-specific fields of a `pg_class` row.
struct ClassRelation<'a> {
    oid: Oid,
    name: &'a str,
    namespace_oid: Oid,
    kind: &'a str,
    relam: i32,
    natts: usize,
    has_index: bool,
    /// Role `ALTER TABLE ... OWNER TO` assigned, when there is one.
    owner: Option<&'a str>,
}

/// A `pg_constraint` row in PostgreSQL's column order; `conkey` carries the
/// one-based attribute numbers in the text form PostgreSQL's arrays use.
fn constraint_row(
    table: &TableDef,
    name: String,
    contype: &str,
    index_oid: Oid,
    key: &str,
) -> Vec<Value> {
    vec![
        Value::Int4(index_oid as i32),
        Value::Text(name),
        Value::Int4(table.schema_oid as i32),
        Value::Text(contype.to_string()),
        Value::Int4(table.oid as i32),
        Value::Int4(index_oid as i32),
        Value::Int4(0),
        Value::Text(format!("{{{key}}}")),
        Value::Null,
        Value::Bool(true),
        Value::Bool(false),
        Value::Bool(false),
        // Foreign-key-only fields: the constraints this engine reports are
        // primary keys and unique indexes, whose FK columns are all NULL.
        Value::Int4(0),     // conparentid
        Value::Null,        // confupdtype
        Value::Null,        // confdeltype
        Value::Null,        // confmatchtype
        Value::Null,        // conpfeqop
        Value::Null,        // conppeqop
        Value::Null,        // conffeqop
        Value::Int4(0),     // coninhcount
        Value::Bool(false), // connoinherit
        Value::Null,        // conbin
        Value::Null,        // conexclop
    ]
}

/// The relation a foreign key points at, as `pg_constraint` reports it.
struct Reference {
    /// `confrelid`: the referenced table.
    table: Oid,
    /// `confkey`: the referenced columns, in PostgreSQL's array text form.
    key: Option<String>,
    /// `confupdtype`, `confdeltype` and `confmatchtype`.
    actions: (&'static str, &'static str, &'static str),
}

/// A `pg_constraint` row for a constraint without a backing index: a foreign
/// key (`contype f`, with `confrelid`/`confkey` and its referential actions) or a
/// check constraint (`c`, whose `conbin` carries the condition text).
fn non_indexed_constraint_row(
    oid: Oid,
    table: &TableDef,
    name: String,
    contype: &str,
    conkey: Option<String>,
    reference: Option<Reference>,
    conbin: Option<String>,
) -> Vec<Value> {
    let (confrelid, confkey, up, del, matching) = match reference {
        Some(reference) => (
            Value::Int4(reference.table as i32),
            reference.key.map(Value::Text).unwrap_or(Value::Null),
            Value::Text(reference.actions.0.to_string()),
            Value::Text(reference.actions.1.to_string()),
            Value::Text(reference.actions.2.to_string()),
        ),
        None => (
            Value::Int4(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ),
    };
    vec![
        Value::Int4(oid as i32),
        Value::Text(name),
        Value::Int4(table.schema_oid as i32),
        Value::Text(contype.to_string()),
        Value::Int4(table.oid as i32),
        Value::Int4(0), // conindid
        confrelid,
        conkey.map(Value::Text).unwrap_or(Value::Null),
        confkey,
        Value::Bool(true),  // convalidated
        Value::Bool(false), // condeferrable
        Value::Bool(false), // condeferred
        Value::Int4(0),     // conparentid
        up,
        del,
        matching,
        Value::Null,        // conpfeqop
        Value::Null,        // conppeqop
        Value::Null,        // conffeqop
        Value::Int4(0),     // coninhcount
        Value::Bool(false), // connoinherit
        conbin.map(Value::Text).unwrap_or(Value::Null),
        Value::Null, // conexclop
    ]
}

/// Materialises `source` from the snapshot, appending the `tableoid` system
/// column that PostgreSQL exposes on every row.
pub fn rows(snapshot: &CatalogSnapshot, source: CatalogSource) -> Vec<Vec<Value>> {
    let mut rows = rows_from(snapshot, source);
    let tableoid = catalog_tableoid(source);
    for row in &mut rows {
        row.push(Value::Int4(tableoid));
    }
    rows
}

/// The `tableoid` value of a catalogue relation: the OID of the relation the
/// row physically belongs to. Real system tables use PostgreSQL's assigned
/// OIDs; views and placeholder relations report `0`, so a client filtering on
/// a system table's OID only sees that system table's rows - the same effect
/// a real view's distinct `tableoid` has.
fn catalog_tableoid(source: CatalogSource) -> i32 {
    match source {
        CatalogSource::Database => 1262,
        CatalogSource::Namespace => 2615,
        CatalogSource::Class => 1259,
        CatalogSource::Attribute => 1249,
        CatalogSource::AttrDef => 2224,
        CatalogSource::Type => 1247,
        CatalogSource::Constraint => 2606,
        CatalogSource::Am => 2601,
        CatalogSource::Index => 2610,
        CatalogSource::Tablespace => 1213,
        _ => 0,
    }
}

/// The per-source row materialisation, without `tableoid`.
fn rows_from(snapshot: &CatalogSnapshot, source: CatalogSource) -> Vec<Vec<Value>> {
    match source {
        CatalogSource::Database => snapshot
            .databases
            .iter()
            .map(|def| {
                vec![
                    Value::Int4(def.oid as i32),
                    Value::Text(def.name.clone()),
                    Value::Int4(snapshot.owner_oid_of(&def.owner) as i32),
                    // Every database is UTF-8; 6 is its encoding number.
                    Value::Int4(6),
                    Value::Bool(def.is_template),
                    Value::Bool(true),
                    Value::Int4(-1),
                    Value::Int4(0),    // datfrozenxid
                    Value::Int4(0),    // datminmxid
                    Value::Int4(1663), // default tablespace
                    Value::Text("C".to_string()),
                    Value::Text("C".to_string()),
                    Value::Text("c".to_string()),
                    Value::Null, // daticulocale
                    Value::Null, // daticurules
                    Value::Null, // datcollversion
                    Value::Null,
                ]
            })
            .collect(),
        CatalogSource::Roles => snapshot
            .roles
            .iter()
            .map(|role| {
                vec![
                    Value::Text(role.name.clone()),
                    Value::Bool(role.superuser),
                    Value::Bool(true),
                    Value::Bool(role.create_role),
                    Value::Bool(role.create_db),
                    Value::Bool(role.can_login),
                    Value::Bool(false),
                    Value::Int4(-1),
                    // Never expose verifiers, not even to superusers: nothing a
                    // client does with them is legitimate.
                    Value::Null,
                    Value::Null,
                    Value::Bool(false),
                    Value::Null,
                    Value::Int4(role.oid as i32),
                ]
            })
            .collect(),
        CatalogSource::User => snapshot
            .roles
            .iter()
            .filter(|role| role.can_login)
            .map(|role| {
                vec![
                    Value::Text(role.name.clone()),
                    Value::Bool(role.superuser),
                    Value::Bool(role.create_db),
                    Value::Bool(false),
                    Value::Bool(false),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ]
            })
            .collect(),
        CatalogSource::Namespace => snapshot
            .database
            .catalog
            .schemas()
            .iter()
            .map(|schema| {
                vec![
                    Value::Int4(schema.oid as i32),
                    Value::Text(schema.name.clone()),
                    Value::Int4(snapshot.owner_oid() as i32),
                    Value::Null,
                ]
            })
            .collect(),
        CatalogSource::Class => {
            let mut rows = Vec::new();
            for table in snapshot.database.catalog.tables() {
                let natts = table.columns.iter().filter(|c| !c.dropped).count();
                let has_index = !table.index_oids().is_empty();
                rows.push(snapshot.class_row(ClassRelation {
                    oid: table.oid,
                    name: &table.name,
                    namespace_oid: table.schema_oid,
                    kind: "r",
                    relam: 0,
                    natts,
                    has_index,
                    owner: table.owner.as_deref(),
                }));
                for index in snapshot.database.catalog.table_indexes(table.oid) {
                    let relam = match index.method {
                        IndexMethod::BTree => BTREE_AM,
                        IndexMethod::Hash => HASH_AM,
                    };
                    // An index belongs to the table's owner: PostgreSQL keeps no
                    // separate owner for it.
                    rows.push(snapshot.class_row(ClassRelation {
                        oid: index.oid,
                        name: &index.name,
                        namespace_oid: table.schema_oid,
                        kind: "i",
                        relam,
                        natts: index.columns.len(),
                        has_index: false,
                        owner: table.owner.as_deref(),
                    }));
                }
            }
            for schema in snapshot.database.catalog.schemas() {
                for name in snapshot.database.views_in(schema.oid) {
                    rows.push(snapshot.class_row(ClassRelation {
                        oid: view_oid(schema.oid, &name),
                        name: &name,
                        namespace_oid: schema.oid,
                        kind: "v",
                        relam: 0,
                        natts: 0,
                        has_index: false,
                        owner: None,
                    }));
                }
            }
            // Sequences are relations in PostgreSQL (`relkind = 'S'`); browsers
            // list them from `pg_class` and read the numbers from `pg_sequence`.
            for sequence in snapshot.database.catalog.sequences() {
                rows.push(snapshot.class_row(ClassRelation {
                    oid: sequence.oid,
                    name: &sequence.name,
                    namespace_oid: sequence.schema_oid,
                    kind: "S",
                    relam: 0,
                    natts: 0,
                    has_index: false,
                    owner: None,
                }));
            }
            rows
        }
        CatalogSource::Attribute => snapshot
            .database
            .catalog
            .tables()
            .iter()
            .flat_map(|table| {
                table
                    .columns
                    .iter()
                    .filter(|column| !column.dropped)
                    .map(move |column| {
                        vec![
                            Value::Int4(table.oid as i32),
                            Value::Text(column.name.clone()),
                            Value::Int4(column.data_type.oid() as i32),
                            Value::Int4(typlen(&column.data_type)),
                            Value::Int4(column.ordinal as i32 + 1),
                            Value::Int4(-1), // attcacheoff
                            Value::Int4(atttypmod(&column.data_type)),
                            Value::Int4(0), // attndims
                            Value::Bool(matches!(
                                column.data_type,
                                DataType::Bool | DataType::Int2 | DataType::Int4 | DataType::Float4
                            )),
                            Value::Text("i".to_string()), // attalign
                            Value::Text("p".to_string()), // attstorage
                            Value::Text(String::new()),   // attcompression
                            Value::Bool(!column.nullable),
                            Value::Bool(column.default.is_some()),
                            Value::Bool(false), // atthasmissing
                            Value::Text(String::new()),
                            Value::Text(String::new()),
                            Value::Bool(false),
                            Value::Bool(true), // attislocal
                            Value::Int4(0),    // attinhcount
                            Value::Int4(0),    // attcollation
                            Value::Null,       // attstattarget
                            Value::Null,       // attacl
                            Value::Null,       // attoptions
                            Value::Null,       // attfdwoptions
                            Value::Null,       // attmissingval
                        ]
                    })
            })
            .collect(),
        CatalogSource::AttrDef => snapshot
            .database
            .catalog
            .tables()
            .iter()
            .flat_map(|table| {
                table
                    .columns
                    .iter()
                    .filter(|column| !column.dropped)
                    .filter_map(|column| {
                        let source = column.default.as_ref()?;
                        let adnum = column.ordinal as i32 + 1;
                        Some(vec![
                            Value::Int4(attrdef_oid(table.oid, adnum) as i32),
                            Value::Int4(table.oid as i32),
                            Value::Int4(adnum),
                            // PostgreSQL stores a `pg_node_tree` here; the engine
                            // keeps defaults already rendered to SQL text, which
                            // is exactly what `pg_get_expr` would display.
                            Value::Text(source.clone()),
                        ])
                    })
            })
            .collect(),
        CatalogSource::Type => {
            let owner = snapshot.superuser_oid as i32;
            let mut rows: Vec<Vec<Value>> = BUILTIN_TYPES
                .iter()
                .map(|(name, data_type, category)| type_row(name, data_type, category, owner))
                .collect();
            // PostgreSQL registers an array type for every base type, named with
            // an underscore prefix. Browsers join `pg_type.typelem` against them
            // to name the element type of an array column, and a column declared
            // `int4[]` reports such a row's OID as its `atttypid`.
            rows.extend(
                BUILTIN_TYPES
                    .iter()
                    .filter(|(_, data_type, _)| data_type.array_oid() != 0)
                    .map(|(name, data_type, _)| {
                        type_row(
                            &format!("_{name}"),
                            &DataType::Array(Box::new(data_type.clone())),
                            "A",
                            owner,
                        )
                    }),
            );
            rows
        }
        CatalogSource::Settings => snapshot
            .settings
            .iter()
            .map(|(name, setting)| {
                let vartype = if setting == "on" || setting == "off" {
                    "bool"
                } else if setting.parse::<i64>().is_ok() {
                    "integer"
                } else {
                    "string"
                };
                vec![
                    Value::Text(name.clone()),
                    Value::Text(setting.clone()),
                    Value::Null,
                    Value::Text("VelocitySQL".to_string()),
                    Value::Text("configuration parameter".to_string()),
                    Value::Null,
                    Value::Text("user".to_string()),
                    Value::Text(vartype.to_string()),
                    Value::Text("default".to_string()),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Text(setting.clone()),
                    Value::Text(setting.clone()),
                    Value::Bool(false),
                ]
            })
            .collect(),
        CatalogSource::Tables => snapshot
            .database
            .catalog
            .tables()
            .iter()
            .map(|table| {
                vec![
                    Value::Text(snapshot.schema_name(table.schema_oid)),
                    Value::Text(table.name.clone()),
                    Value::Text(snapshot.relation_owner_name(table.owner.as_deref())),
                    Value::Null, // tablespace
                    Value::Bool(!table.index_oids().is_empty()),
                    Value::Bool(false),
                    Value::Bool(false),
                    Value::Bool(false),
                ]
            })
            .collect(),
        CatalogSource::Views => snapshot
            .database
            .catalog
            .schemas()
            .iter()
            .flat_map(|schema| {
                let schema_name = schema.name.clone();
                snapshot
                    .database
                    .views_in(schema.oid)
                    .into_iter()
                    .map(move |name| {
                        // The parsed query is kept verbatim, so the definition
                        // renders back to the SELECT the user wrote.
                        let definition = snapshot
                            .database
                            .view(&[schema_name.clone(), name.clone()])
                            .map(|query| render::query(&query))
                            .unwrap_or_default();
                        vec![
                            Value::Text(schema_name.clone()),
                            Value::Text(name),
                            Value::Text(snapshot.owner_name()),
                            Value::Text(definition),
                        ]
                    })
            })
            .collect(),
        CatalogSource::Indexes => snapshot
            .database
            .catalog
            .tables()
            .iter()
            .flat_map(|table| {
                let schema_name = snapshot.schema_name(table.schema_oid);
                let table_name = table.name.clone();
                snapshot
                    .database
                    .catalog
                    .table_indexes(table.oid)
                    .into_iter()
                    .map(move |index| {
                        vec![
                            Value::Text(schema_name.clone()),
                            Value::Text(table_name.clone()),
                            Value::Text(index.name.clone()),
                            Value::Null,
                            Value::Text(index_def_sql(table, &index)),
                        ]
                    })
            })
            .collect(),
        CatalogSource::Sequences => snapshot
            .database
            .catalog
            .sequences()
            .iter()
            .map(|sequence| {
                // `last_value` is `NULL` until the first `nextval`, exactly as
                // PostgreSQL reports a fresh sequence.
                let last = sequence.last_value.load(Ordering::SeqCst);
                let advanced = last != sequence.start - sequence.increment;
                vec![
                    Value::Text(snapshot.schema_name(sequence.schema_oid)),
                    Value::Text(sequence.name.clone()),
                    Value::Text(snapshot.owner_name()),
                    Value::Text("bigint".to_string()),
                    Value::Int8(sequence.start),
                    Value::Int8(sequence.min_value),
                    Value::Int8(sequence.max_value),
                    Value::Int8(sequence.increment),
                    Value::Bool(false), // cycle: not supported yet
                    Value::Int8(1),     // cache: single-slot
                    if advanced {
                        Value::Int8(last)
                    } else {
                        Value::Null
                    },
                ]
            })
            .collect(),
        CatalogSource::Sequence => snapshot
            .database
            .catalog
            .sequences()
            .iter()
            .map(|sequence| {
                vec![
                    Value::Int4(sequence.oid as i32),
                    Value::Int4(DataType::Int8.oid() as i32),
                    Value::Int8(sequence.start),
                    Value::Int8(sequence.increment),
                    Value::Int8(sequence.max_value),
                    Value::Int8(sequence.min_value),
                    Value::Int8(1),     // seqcache
                    Value::Bool(false), // seqcycle
                ]
            })
            .collect(),
        CatalogSource::Tablespace => vec![
            // The two tablespaces every cluster has.
            vec![
                Value::Int4(1663),
                Value::Text("pg_default".to_string()),
                Value::Int4(snapshot.owner_oid() as i32),
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Int4(1664),
                Value::Text("pg_global".to_string()),
                Value::Int4(snapshot.owner_oid() as i32),
                Value::Null,
                Value::Null,
            ],
        ],
        // Constraints are real metadata: every primary key and unique index is
        // reported the way PostgreSQL reports it.
        CatalogSource::Constraint => snapshot
            .database
            .catalog
            .tables()
            .iter()
            .flat_map(|table| {
                let indexes = snapshot.database.catalog.table_indexes(table.oid);
                let mut rows = Vec::new();
                if !table.primary_key.is_empty() {
                    let primary = indexes.iter().find(|index| index.primary);
                    let (oid, key) = match primary {
                        Some(index) => (index.oid, key_literal(&index.columns)),
                        None => (0, key_literal(&table.primary_key)),
                    };
                    rows.push(constraint_row(
                        table,
                        format!("{}_pkey", table.name),
                        "p",
                        oid,
                        &key,
                    ));
                }
                for index in indexes
                    .iter()
                    .filter(|index| index.unique && !index.primary)
                {
                    rows.push(constraint_row(
                        table,
                        index.name.clone(),
                        "u",
                        index.oid,
                        &key_literal(&index.columns),
                    ));
                }
                for key in &table.foreign_keys {
                    rows.push(non_indexed_constraint_row(
                        key.oid,
                        table,
                        key.name.clone(),
                        "f",
                        Some(format!("{{{}}}", key_literal(&key.columns))),
                        Some(Reference {
                            table: key.ref_table,
                            key: Some(format!("{{{}}}", key_literal(&key.ref_columns))),
                            // NO ACTION on both sides, MATCH SIMPLE: the actions
                            // the engine records.
                            actions: ("a", "a", "s"),
                        }),
                        None,
                    ));
                }
                for check in &table.checks {
                    rows.push(non_indexed_constraint_row(
                        check.oid,
                        table,
                        check.name.clone(),
                        "c",
                        None,
                        None,
                        Some(check.expr.clone()),
                    ));
                }
                rows
            })
            .collect(),
        CatalogSource::Am => vec![
            vec![
                Value::Int4(BTREE_AM),
                Value::Text("btree".to_string()),
                Value::Text("btree handler".to_string()),
                Value::Text("i".to_string()),
            ],
            vec![
                Value::Int4(HASH_AM),
                Value::Text("hash".to_string()),
                Value::Text("hash handler".to_string()),
                Value::Text("i".to_string()),
            ],
        ],
        // `indkey`/`indcollation`/`indclass` are int2/oid vectors: PostgreSQL's
        // text form separates the elements with spaces, which is what a driver
        // round-tripping them expects.
        CatalogSource::Index => snapshot
            .database
            .catalog
            .tables()
            .iter()
            .flat_map(|table| {
                snapshot
                    .database
                    .catalog
                    .table_indexes(table.oid)
                    .into_iter()
                    .map(move |index| {
                        let positions = || {
                            index
                                .columns
                                .iter()
                                .map(|&ordinal| (ordinal + 1).to_string())
                                .collect::<Vec<_>>()
                                .join(" ")
                        };
                        vec![
                            Value::Int4(index.oid as i32),
                            Value::Int4(index.table_oid as i32),
                            Value::Int4(index.columns.len() as i32),
                            Value::Int4(index.columns.len() as i32),
                            Value::Bool(index.unique),
                            Value::Bool(false),
                            Value::Bool(index.primary),
                            Value::Bool(false),
                            Value::Bool(true),
                            Value::Bool(false),
                            Value::Bool(true),
                            Value::Bool(false),
                            Value::Bool(true),
                            Value::Bool(true),
                            Value::Bool(false),
                            Value::Text(positions()),
                            Value::Text(zero_vector(index.columns.len())),
                            Value::Text(zero_vector(index.columns.len())),
                            Value::Text(zero_vector(index.columns.len())),
                            Value::Null,
                            Value::Null,
                        ]
                    })
            })
            .collect(),
        CatalogSource::Schemata => snapshot
            .database
            .catalog
            .schemas()
            .iter()
            .map(|schema| {
                vec![
                    Value::Text(snapshot.database.name().to_string()),
                    Value::Text(schema.name.clone()),
                    Value::Text(snapshot.owner_name()),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ]
            })
            .collect(),
        CatalogSource::IsTables => snapshot
            .database
            .catalog
            .schemas()
            .iter()
            .flat_map(|schema| {
                let catalog = snapshot.database.name().to_string();
                let schema_name = schema.name.clone();
                let mut rows = Vec::new();
                for table in snapshot
                    .database
                    .catalog
                    .tables()
                    .iter()
                    .filter(|table| table.schema_oid == schema.oid)
                {
                    rows.push(vec![
                        Value::Text(catalog.clone()),
                        Value::Text(schema_name.clone()),
                        Value::Text(table.name.clone()),
                        Value::Text("BASE TABLE".to_string()),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text("YES".to_string()),
                        Value::Text("NO".to_string()),
                        Value::Null,
                    ]);
                }
                for name in snapshot.database.views_in(schema.oid) {
                    rows.push(vec![
                        Value::Text(catalog.clone()),
                        Value::Text(schema_name.clone()),
                        Value::Text(name),
                        Value::Text("VIEW".to_string()),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text("NO".to_string()),
                        Value::Text("NO".to_string()),
                        Value::Null,
                    ]);
                }
                rows
            })
            .collect(),
        CatalogSource::IsColumns => snapshot
            .database
            .catalog
            .tables()
            .iter()
            .flat_map(|table| {
                let catalog = snapshot.database.name().to_string();
                let schema_name = snapshot.schema_name(table.schema_oid);
                table
                    .columns
                    .iter()
                    .filter(|column| !column.dropped)
                    .map(move |column| {
                        let data_type = &column.data_type;
                        let (max_length, octet_length) = match data_type {
                            DataType::Varchar(Some(len)) | DataType::Char(Some(len)) => {
                                (Some(*len as i32), Some(*len as i32 * 4))
                            }
                            _ => (None, None),
                        };
                        let (precision, radix, scale) = match data_type {
                            DataType::Int2 => (Some(16), Some(2), None),
                            DataType::Int4 => (Some(32), Some(2), None),
                            DataType::Int8 => (Some(64), Some(2), None),
                            DataType::Float4 => (Some(24), Some(2), None),
                            DataType::Float8 => (Some(53), Some(2), None),
                            DataType::Numeric { precision, scale } => (
                                precision.map(|p| p as i32),
                                Some(10),
                                scale.map(|s| s as i32),
                            ),
                            _ => (None, None, None),
                        };
                        let datetime_precision = match data_type {
                            DataType::Time
                            | DataType::Timestamp
                            | DataType::Timestamptz
                            | DataType::Interval => Some(6),
                            _ => None,
                        };
                        let interval_precision = match data_type {
                            DataType::Interval => Some(6),
                            _ => None,
                        };
                        let ordinal = column.ordinal as i32 + 1;
                        vec![
                            Value::Text(catalog.clone()),     // table_catalog
                            Value::Text(schema_name.clone()), // table_schema
                            Value::Text(table.name.clone()),  // table_name
                            Value::Text(column.name.clone()), // column_name
                            Value::Int4(ordinal),             // ordinal_position
                            column
                                .default
                                .clone()
                                .map(Value::Text)
                                .unwrap_or(Value::Null), // column_default
                            Value::Text(if column.nullable { "YES" } else { "NO" }.to_string()), // is_nullable
                            Value::Text(standard_data_type(data_type)), // data_type
                            max_length.map(Value::Int4).unwrap_or(Value::Null), // character_maximum_length
                            octet_length.map(Value::Int4).unwrap_or(Value::Null), // character_octet_length
                            precision.map(Value::Int4).unwrap_or(Value::Null), // numeric_precision
                            radix.map(Value::Int4).unwrap_or(Value::Null), // numeric_precision_radix
                            scale.map(Value::Int4).unwrap_or(Value::Null), // numeric_scale
                            datetime_precision.map(Value::Int4).unwrap_or(Value::Null), // datetime_precision
                            Value::Null, // interval_type
                            interval_precision.map(Value::Int4).unwrap_or(Value::Null), // interval_precision
                            Value::Null, // character_set_catalog
                            Value::Null, // character_set_schema
                            Value::Null, // character_set_name
                            Value::Null, // collation_catalog
                            Value::Null, // collation_schema
                            Value::Null, // collation_name
                            Value::Null, // domain_catalog
                            Value::Null, // domain_schema
                            Value::Null, // domain_name
                            Value::Null, // udt_catalog
                            Value::Text("pg_catalog".to_string()), // udt_schema
                            Value::Text(pg_internal_type_name(data_type)), // udt_name
                            Value::Null, // scope_catalog
                            Value::Null, // scope_schema
                            Value::Null, // scope_name
                            Value::Null, // maximum_cardinality
                            Value::Text(ordinal.to_string()), // dtd_identifier
                            Value::Text("NO".to_string()), // is_self_referencing
                            Value::Text("NO".to_string()), // is_identity
                            Value::Null, // identity_generation
                            Value::Null, // identity_start
                            Value::Null, // identity_increment
                            Value::Null, // identity_maximum
                            Value::Null, // identity_minimum
                            Value::Null, // identity_cycle
                            Value::Text("NEVER".to_string()), // is_generated
                            Value::Null, // generation_expression
                            Value::Text("YES".to_string()), // is_updatable
                        ]
                    })
            })
            .collect(),
        CatalogSource::IsViews => snapshot
            .database
            .catalog
            .schemas()
            .iter()
            .flat_map(|schema| {
                let catalog = snapshot.database.name().to_string();
                let schema_name = schema.name.clone();
                snapshot
                    .database
                    .views_in(schema.oid)
                    .into_iter()
                    .map(move |name| {
                        vec![
                            Value::Text(catalog.clone()),
                            Value::Text(schema_name.clone()),
                            Value::Text(name),
                            Value::Text(String::new()),
                            Value::Text("NONE".to_string()),
                            Value::Text("NO".to_string()),
                            Value::Text("NO".to_string()),
                            Value::Text("NO".to_string()),
                            Value::Text("NO".to_string()),
                            Value::Text("NO".to_string()),
                        ]
                    })
            })
            .collect(),
        // Constraint listings share one enumeration: primary keys and unique
        // indexes, exactly what `pg_constraint` reports.
        CatalogSource::IsTableConstraints | CatalogSource::IsKeyColumnUsage => snapshot
            .database
            .catalog
            .tables()
            .iter()
            .flat_map(|table| {
                let catalog = snapshot.database.name().to_string();
                let schema_name = snapshot.schema_name(table.schema_oid);
                let table_name = table.name.clone();
                snapshot
                    .database
                    .catalog
                    .table_indexes(table.oid)
                    .into_iter()
                    .filter(|index| index.primary || index.unique)
                    .flat_map(move |index| {
                        let constraint_name = index.name.clone();
                        let constraint_type = if index.primary {
                            "PRIMARY KEY"
                        } else {
                            "UNIQUE"
                        };
                        let key_columns: Vec<Vec<Value>> = index
                            .columns
                            .iter()
                            .enumerate()
                            .map(|(position, &ordinal)| {
                                let column_name = table
                                    .columns
                                    .iter()
                                    .find(|column| column.ordinal == ordinal)
                                    .map(|column| column.name.clone())
                                    .unwrap_or_default();
                                if source == CatalogSource::IsTableConstraints {
                                    vec![
                                        Value::Text(catalog.clone()),
                                        Value::Text(schema_name.clone()),
                                        Value::Text(constraint_name.clone()),
                                        Value::Text(catalog.clone()),
                                        Value::Text(schema_name.clone()),
                                        Value::Text(table_name.clone()),
                                        Value::Text(constraint_type.to_string()),
                                        Value::Text("NO".to_string()),
                                        Value::Text("NO".to_string()),
                                        Value::Text("YES".to_string()),
                                    ]
                                } else {
                                    vec![
                                        Value::Text(catalog.clone()),
                                        Value::Text(schema_name.clone()),
                                        Value::Text(constraint_name.clone()),
                                        Value::Text(catalog.clone()),
                                        Value::Text(schema_name.clone()),
                                        Value::Text(table_name.clone()),
                                        Value::Text(column_name),
                                        Value::Int4(position as i32 + 1),
                                        Value::Null,
                                    ]
                                }
                            })
                            .collect::<Vec<_>>();
                        key_columns
                    })
            })
            .collect(),
        CatalogSource::Empty(_) => Vec::new(),
    }
}

/// Builds the snapshot for a session's statement.
pub fn snapshot(
    database: &Arc<Database>,
    engine: &Engine,
    settings: Vec<(String, String)>,
    search_path: String,
) -> CatalogSnapshot {
    CatalogSnapshot {
        databases: engine
            .databases()
            .iter()
            .map(|database| database.def().clone())
            .collect(),
        roles: engine.roles(),
        superuser_oid: engine
            .role(engine.superuser_name())
            .map(|role| role.oid)
            .unwrap_or(0),
        database: database.clone(),
        settings,
        search_path,
    }
}
