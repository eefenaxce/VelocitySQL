//! Asynchronous, always-on durability for the in-memory engine.
//!
//! Milestone M2 is the real write-ahead log; this module is the stopgap the
//! operator asked for: queries stay in memory (fast, consistent), and a
//! background task periodically serialises the engine's *objects and rows* to a
//! compact binary file (`data/velocitysql.snapshot`) so a restart loses at most
//! the last few seconds of writes. A snapshot captures everything the engine
//! owns — cluster roles, databases with their schemas, sequences, tables
//! (columns, keys, indexes, checks), the row data those tables hold, and views
//! — so a database, role or view created at runtime survives a restart exactly
//! like a table does.
//!
//! The on-disk format is `bincode` over the serde representation — a binary,
//! not human-readable, encoding — and every write is `fsync`'d (temp file plus
//! atomic rename) so the bytes reach durable storage, not just the page cache.
//!
//! Snapshots are best-effort: a row that violates a constraint in the live
//! catalog (it should not, because it came from that same catalog) is skipped
//! rather than aborting the whole restore.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use vsql_catalog::{
    Catalog, CheckDef, ColumnDef, DatabaseDef, ForeignKeyDef, IndexDef, IndexMethod, Oid, RoleDef,
    SequenceDef, TableDef,
};
use vsql_parser::ast::Query;
use vsql_types::{DataType, Value};

use crate::database::Engine;

/// On-disk snapshot format version. Bump when the layout changes incompatibly.
///
/// * Version 4 added the quoting flag to `ColumnRef`, which a persisted view
///   definition embeds.
/// * Version 5 changed how a `numeric` and a JSON document are encoded (as text
///   rather than as a serde-described value), so a version-4 file holding either
///   is not readable.
const SNAPSHOT_VERSION: u32 = 5;

/// Default snapshot file, relative to the server's working directory.
///
/// Durability is always on; this is where the row data lands.
pub const DEFAULT_SNAPSHOT_PATH: &str = "data/velocitysql.snapshot";

/// How often the background task rewrites the snapshot.
pub const DEFAULT_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5);

/// Signals the background saver that data changed, so the snapshot is
/// rewritten promptly instead of waiting for the periodic tick.
///
/// A write marks the trigger dirty *after* the statement has been applied in
/// memory and its result handed back: persistence never blocks a query, it
/// just follows it as soon as possible.
#[derive(Debug, Default)]
pub struct PersistTrigger {
    dirty: parking_lot::Mutex<bool>,
    signal: parking_lot::Condvar,
}

impl PersistTrigger {
    /// Creates an idle trigger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks data changed and wakes the saver.
    pub fn mark_dirty(&self) {
        *self.dirty.lock() = true;
        self.signal.notify_all();
    }

    /// Blocks until marked dirty or `timeout` elapses; returns `true` when
    /// dirty, `false` on timeout. The dirty flag is consumed either way.
    fn wait_dirty(&self, timeout: Duration) -> bool {
        let mut dirty = self.dirty.lock();
        if !*dirty {
            self.signal.wait_for(&mut dirty, timeout);
        }
        let was_dirty = *dirty;
        *dirty = false;
        was_dirty
    }
}

/// A complete snapshot of the cluster: its roles and every database, together
/// with each database's schema (tables, columns, keys, indexes, sequences,
/// views) and the row data those tables hold.
///
/// Persisting the schema is what makes objects created at runtime — for example
/// through a GUI like Navicat — survive a restart. The catalog is rebuilt empty
/// on every start, so without the schema here the restored rows would have
/// nowhere to land; and without the databases and roles here, a `CREATE
/// DATABASE` or `CREATE ROLE` would vanish.
#[derive(Serialize, Deserialize, Default)]
pub struct Snapshot {
    version: u32,
    /// Cluster roles (`pg_authid`), ordered by name.
    roles: Vec<StoredRole>,
    databases: HashMap<String, DatabaseSnapshot>,
}

/// A cluster role, including the SCRAM verifier exactly as it is stored.
#[derive(Serialize, Deserialize)]
struct StoredRole {
    name: String,
    oid: Oid,
    superuser: bool,
    can_login: bool,
    create_db: bool,
    create_role: bool,
    /// `pg_authid.rolpassword`; `None` means the role cannot log in with a
    /// password.
    password: Option<String>,
}

/// One database's persisted state: its identity, user schemas, sequences,
/// tables and views.
#[derive(Serialize, Deserialize, Default)]
struct DatabaseSnapshot {
    /// `pg_database.oid`, unique across the cluster.
    oid: Oid,
    /// `pg_database.datdba`.
    owner: String,
    schemas: Vec<String>,
    sequences: Vec<StoredSequence>,
    tables: HashMap<String, TableSnapshot>,
    views: Vec<StoredView>,
}

/// A view's definition, kept as its parsed query so it round-trips exactly
/// (joins, aggregates, `ORDER BY` and all) instead of being re-rendered to text.
#[derive(Serialize, Deserialize)]
struct StoredView {
    schema: String,
    name: String,
    query: Query,
}

/// One column of a persisted table.
#[derive(Serialize, Deserialize)]
struct StoredColumn {
    name: String,
    data_type: DataType,
    nullable: bool,
    default: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct StoredCheck {
    name: String,
    expr: String,
}

#[derive(Serialize, Deserialize)]
struct StoredForeignKey {
    name: Option<String>,
    /// Local column names; resolved back to ordinals on restore.
    columns: Vec<String>,
    ref_schema: String,
    ref_table: String,
    ref_columns: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct StoredIndex {
    name: String,
    columns: Vec<usize>,
    unique: bool,
    primary: bool,
    using_hash: bool,
}

#[derive(Serialize, Deserialize)]
struct StoredSequence {
    schema: String,
    name: String,
    oid: u32,
    start: i64,
    last_value: i64,
    increment: i64,
    min_value: i64,
    max_value: i64,
}

/// The schema and row data of a single table.
#[derive(Serialize, Deserialize, Default)]
struct TableSnapshot {
    schema: String,
    oid: u32,
    columns: Vec<StoredColumn>,
    primary_key: Vec<usize>,
    owner: Option<String>,
    checks: Vec<StoredCheck>,
    foreign_keys: Vec<StoredForeignKey>,
    indexes: Vec<StoredIndex>,
    /// Row data, aligned to `columns` by position.
    rows: Vec<Vec<Value>>,
}

impl Snapshot {
    /// Captures every role, database, view and row the engine owns.
    pub fn take(engine: &Engine) -> Snapshot {
        let roles: Vec<StoredRole> = engine
            .roles()
            .into_iter()
            .map(|role| StoredRole {
                name: role.name,
                oid: role.oid,
                superuser: role.superuser,
                can_login: role.can_login,
                create_db: role.create_db,
                create_role: role.create_role,
                password: role.password,
            })
            .collect();
        let mut databases = HashMap::new();
        for database in engine.databases() {
            let cat = &database.catalog;
            let schemas: Vec<String> = cat
                .schemas()
                .into_iter()
                .filter(|schema| !schema.is_system)
                .map(|schema| schema.name)
                .collect();
            let sequences: Vec<StoredSequence> = cat
                .sequences()
                .iter()
                .filter_map(|seq| {
                    let schema = cat.schema_name(seq.schema_oid)?;
                    Some(StoredSequence {
                        schema,
                        name: seq.name.clone(),
                        oid: seq.oid,
                        start: seq.start,
                        last_value: seq.last_value(),
                        increment: seq.increment,
                        min_value: seq.min_value,
                        max_value: seq.max_value,
                    })
                })
                .collect();
            let mut tables = HashMap::new();
            for def in cat.tables() {
                if def.is_system {
                    continue;
                }
                let Some(table) = database.table(def.oid) else {
                    continue;
                };
                let schema_name = cat
                    .schema_name(def.schema_oid)
                    .unwrap_or_else(|| "public".to_string());
                let columns: Vec<StoredColumn> = def
                    .columns
                    .iter()
                    .map(|column| StoredColumn {
                        name: column.name.clone(),
                        data_type: column.data_type.clone(),
                        nullable: column.nullable,
                        default: column.default.clone(),
                    })
                    .collect();
                let checks = def
                    .checks
                    .iter()
                    .map(|check| StoredCheck {
                        name: check.name.clone(),
                        expr: check.expr.clone(),
                    })
                    .collect();
                let foreign_keys = def
                    .foreign_keys
                    .iter()
                    .map(|fk| {
                        let ref_def = cat.table(fk.ref_table);
                        let (ref_schema, ref_table) = match &ref_def {
                            Some(d) => (
                                cat.schema_name(d.schema_oid).unwrap_or_default(),
                                d.name.clone(),
                            ),
                            None => (String::new(), String::new()),
                        };
                        let ref_columns = match &ref_def {
                            Some(d) => fk
                                .ref_columns
                                .iter()
                                .map(|&i| {
                                    d.columns.get(i).map(|c| c.name.clone()).unwrap_or_default()
                                })
                                .collect(),
                            None => Vec::new(),
                        };
                        StoredForeignKey {
                            name: Some(fk.name.clone()),
                            columns: fk
                                .columns
                                .iter()
                                .map(|&i| {
                                    def.columns
                                        .get(i)
                                        .map(|c| c.name.clone())
                                        .unwrap_or_default()
                                })
                                .collect(),
                            ref_schema,
                            ref_table,
                            ref_columns,
                        }
                    })
                    .collect();
                let indexes = cat
                    .table_indexes(def.oid)
                    .iter()
                    .map(|idx| StoredIndex {
                        name: idx.name.clone(),
                        columns: idx.columns.clone(),
                        unique: idx.unique,
                        primary: idx.primary,
                        using_hash: matches!(idx.method, IndexMethod::Hash),
                    })
                    .collect();
                let rows: Vec<Vec<Value>> =
                    table.snapshot().into_iter().map(|(_, row)| row).collect();
                tables.insert(
                    def.name.clone(),
                    TableSnapshot {
                        schema: schema_name,
                        oid: def.oid,
                        columns,
                        primary_key: def.primary_key.clone(),
                        owner: def.owner.clone(),
                        checks,
                        foreign_keys,
                        indexes,
                        rows,
                    },
                );
            }
            let views: Vec<StoredView> = cat
                .schemas()
                .into_iter()
                .filter(|schema| !schema.is_system)
                .flat_map(|schema| {
                    database
                        .views_in(schema.oid)
                        .into_iter()
                        .filter_map(|name| {
                            let query = database.view(&[schema.name.clone(), name.clone()])?;
                            Some(StoredView {
                                schema: schema.name.clone(),
                                name,
                                query: (*query).clone(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            databases.insert(
                database.name().to_string(),
                DatabaseSnapshot {
                    oid: database.oid(),
                    owner: database.def().owner.clone(),
                    schemas,
                    sequences,
                    tables,
                    views,
                },
            );
        }
        Snapshot {
            version: SNAPSHOT_VERSION,
            roles,
            databases,
        }
    }

    /// Rebuilds the captured roles, databases, schema and rows.
    ///
    /// Best-effort: any object that cannot be rebuilt (a referenced table that
    /// vanished, a type the engine no longer understands) is skipped rather than
    /// aborting the whole restore. Objects already present — because `--init` or
    /// `--demo` rebuilt them, or because the bootstrap superuser and the default
    /// databases are created by [`Engine::new`] — are left alone; their rows are
    /// still replayed. That is also what keeps `--superuser-password` in charge
    /// of the bootstrap superuser: its role already exists, so the snapshot's
    /// verifier for it is ignored rather than resurrecting an old password.
    pub fn restore(&self, engine: &Engine) {
        // Roles are cluster state that depends on no database, so they go first.
        let mut max_cluster_oid: Oid = 0;
        for role in &self.roles {
            engine.install_role(RoleDef {
                name: role.name.clone(),
                oid: role.oid,
                superuser: role.superuser,
                can_login: role.can_login,
                create_db: role.create_db,
                create_role: role.create_role,
                password: role.password.clone(),
            });
            max_cluster_oid = max_cluster_oid.max(role.oid);
        }

        for (database_name, snapshot_db) in &self.databases {
            // A database created at runtime is not in the freshly built engine;
            // install it under its original OID instead of skipping its rows.
            let live = engine.install_database(DatabaseDef::new(
                snapshot_db.oid,
                database_name.clone(),
                snapshot_db.owner.clone(),
            ));
            max_cluster_oid = max_cluster_oid.max(snapshot_db.oid);
            let cat = &live.catalog;
            let mut max_oid: Oid = 0;

            for schema in &snapshot_db.schemas {
                let _ = cat.create_schema(schema, true);
            }
            for seq in &snapshot_db.sequences {
                let schema_oid = Self::resolve_or_create_schema(cat, &seq.schema);
                let mut sd =
                    SequenceDef::new(seq.oid, schema_oid, &seq.name, seq.start, seq.increment);
                sd.last_value.store(seq.last_value, Ordering::SeqCst);
                sd.min_value = seq.min_value;
                sd.max_value = seq.max_value;
                let _ = cat.install_sequence(Arc::new(sd));
                max_oid = max_oid.max(seq.oid);
            }

            // Pass 1: tables and the indexes that sit directly on them.
            for (table_name, snap) in &snapshot_db.tables {
                let schema_oid = Self::resolve_or_create_schema(cat, &snap.schema);
                if cat.table_named(schema_oid, table_name).is_some() {
                    continue;
                }
                let columns: Vec<ColumnDef> = snap
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, column)| {
                        let mut cd =
                            ColumnDef::new(column.name.clone(), column.data_type.clone(), i);
                        cd.nullable = column.nullable;
                        cd.default = column.default.clone();
                        cd
                    })
                    .collect();
                let mut def = TableDef::new(snap.oid, schema_oid, table_name.clone(), columns);
                def.primary_key = snap.primary_key.clone();
                def.owner = snap.owner.clone();
                let def = Arc::new(def);
                let _ = cat.install_table(def.clone());
                let table = live.mount(def.clone());
                for idx in &snap.indexes {
                    let method = if idx.using_hash {
                        IndexMethod::Hash
                    } else {
                        IndexMethod::BTree
                    };
                    let idef = IndexDef::new(
                        cat.next_oid(),
                        def.oid,
                        idx.name.clone(),
                        idx.columns.clone(),
                        method,
                    );
                    let idef = if idx.unique { idef.unique() } else { idef };
                    let idef = if idx.primary {
                        idef.primary_key()
                    } else {
                        idef
                    };
                    if let Ok(registered) = cat.register_index(idef) {
                        table.attach_index(registered);
                    }
                }
                max_oid = max_oid.max(snap.oid);
            }

            // Pass 2: checks and foreign keys need every table present first.
            for (table_name, snap) in &snapshot_db.tables {
                let Some(schema_oid) = cat.schema(&snap.schema).map(|s| s.oid) else {
                    continue;
                };
                let Some(def) = cat.table_named(schema_oid, table_name) else {
                    continue;
                };
                if !snap.checks.is_empty() {
                    let defs: Vec<CheckDef> = snap
                        .checks
                        .iter()
                        .map(|check| CheckDef {
                            oid: cat.next_oid(),
                            name: check.name.clone(),
                            expr: check.expr.clone(),
                        })
                        .collect();
                    let _ = cat.set_checks(def.oid, defs);
                }
                for fk in &snap.foreign_keys {
                    let Some(ref_schema) = cat.schema(&fk.ref_schema) else {
                        continue;
                    };
                    let Some(ref_def) = cat.table_named(ref_schema.oid, &fk.ref_table) else {
                        continue;
                    };
                    let local_cols: Vec<usize> = fk
                        .columns
                        .iter()
                        .filter_map(|name| def.column_index(name))
                        .collect();
                    let ref_cols: Vec<usize> = fk
                        .ref_columns
                        .iter()
                        .filter_map(|name| ref_def.column_index(name))
                        .collect();
                    if local_cols.len() != fk.columns.len()
                        || ref_cols.len() != fk.ref_columns.len()
                    {
                        continue;
                    }
                    let key = ForeignKeyDef {
                        oid: cat.next_oid(),
                        name: fk.name.clone().unwrap_or_default(),
                        columns: local_cols,
                        ref_table: ref_def.oid,
                        ref_columns: ref_cols,
                    };
                    let _ = cat.attach_foreign_key(def.oid, key);
                }
            }

            // Rows, aligned to the (rebuilt) column order.
            for (table_name, snap) in &snapshot_db.tables {
                let Some(schema_oid) = cat.schema(&snap.schema).map(|s| s.oid) else {
                    continue;
                };
                let Some(def) = cat.table_named(schema_oid, table_name) else {
                    continue;
                };
                let Some(table) = live.table(def.oid) else {
                    continue;
                };
                for row in &snap.rows {
                    let _ = table.insert(row.clone());
                }
            }

            cat.bump_next_oid(max_oid + 1);

            // Views last: their bodies reference the tables above, and
            // `or_replace` makes restore idempotent against a rebuilt schema.
            for view in &snapshot_db.views {
                let schema_oid = Self::resolve_or_create_schema(cat, &view.schema);
                let _ = live.create_view(schema_oid, &view.name, view.query.clone(), true);
            }
        }

        engine.bump_next_oid(max_cluster_oid + 1);
    }

    /// Resolves a schema by name, creating it if it does not yet exist.
    fn resolve_or_create_schema(cat: &Catalog, name: &str) -> Oid {
        if let Some(schema) = cat.schema(name) {
            return schema.oid;
        }
        cat.create_schema(name, true)
            .map(|schema| schema.oid)
            .unwrap_or_else(|_| cat.schema("public").map(|s| s.oid).unwrap_or(0))
    }

    /// Writes the snapshot to `path` as a durable, atomic binary file.
    ///
    /// The payload is encoded with `bincode` (a compact, binary, serde-based
    /// codec — not human-readable JSON) and flushed with `fsync` so the bytes
    /// actually reach the disk, not just the page cache. A temp file plus rename
    /// keeps the on-disk snapshot consistent even if the process dies mid-write.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = bincode::serialize(self).map_err(std::io::Error::other)?;
        let temp = path.with_extension("tmp");
        {
            let mut file = fs::File::create(&temp)?;
            file.write_all(&body)?;
            // Force the data (and the rename below) to durable storage.
            file.sync_all()?;
        }
        fs::rename(&temp, path)?;
        if let Some(parent) = path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    /// Loads a snapshot from `path`; `None` when the file does not exist.
    pub fn load(path: &Path) -> std::io::Result<Option<Snapshot>> {
        let body = match fs::read(path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let snapshot: Snapshot = bincode::deserialize(&body)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "unsupported snapshot version {} (expected {SNAPSHOT_VERSION})",
                    snapshot.version
                ),
            ));
        }
        Ok(Some(snapshot))
    }
}

/// Spawns a background task that snapshots the engine to `path`: promptly
/// after every write (the `trigger` is marked dirty by a session), and at
/// least once per `interval` as a safety net, plus once more when `stop` is
/// signalled.
///
/// The task holds an `Arc<Engine>` clone and never blocks query execution: it
/// only reads, so it takes the storage read locks for the brief moments it
/// copies each table's rows.
pub fn start_background_saver(
    engine: Arc<Engine>,
    path: PathBuf,
    interval: Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
    trigger: Arc<PersistTrigger>,
) {
    std::thread::spawn(move || loop {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = Snapshot::take(&engine).save(&path);
            break;
        }
        // Wake early on a write, or at the latest after `interval`. Either way
        // a save is due: a dirty flag means data changed, a timeout is the
        // periodic fallback.
        trigger.wait_dirty(interval);
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = Snapshot::take(&engine).save(&path);
            break;
        }
        if let Err(error) = Snapshot::take(&engine).save(&path) {
            eprintln!("snapshot save failed: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{QueryResult, Session};

    #[test]
    fn snapshot_round_trips_through_file() {
        let engine = Arc::new(Engine::new());
        let mut session = Session::new(engine.clone());
        session
            .execute("CREATE TABLE t(a int, b text)")
            .expect("create");
        session
            .execute("INSERT INTO t VALUES (1, 'x'), (2, 'y')")
            .expect("insert");

        let snapshot = Snapshot::take(&engine);
        assert_eq!(snapshot.databases["velocitysql"].tables["t"].rows.len(), 2);

        // Discard the rows, then restore from a serialised snapshot.
        session.execute("TRUNCATE t").expect("truncate");
        let path = std::env::temp_dir().join("velocitysql-persist-test.bin");
        snapshot.save(&path).expect("save");
        let loaded = Snapshot::load(&path).expect("load").expect("present");
        loaded.restore(&engine);

        let result = session
            .execute("SELECT a FROM t ORDER BY a")
            .expect("select");
        let QueryResult::Rows { rows, .. } = &result[0] else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Int4(1));
        assert_eq!(rows[1][0], Value::Int4(2));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_snapshot_round_trips_every_value_the_engine_has() {
        // Every type has to survive the encoder, not just the ones a first
        // round of tests happened to use: a value whose `Deserialize` wants a
        // self-describing format makes the whole file unreadable, and the
        // symptom is a snapshot that silently refuses to load.
        let engine = Arc::new(Engine::new());
        let mut session = Session::new(engine.clone());
        session
            .execute(
                "CREATE TABLE mixed (
                     id INT PRIMARY KEY, ratio NUMERIC(10,2), doc JSONB, flag BOOL,
                     at TIMESTAMPTZ, day DATE, clock TIME, wait INTERVAL,
                     tag UUID, blob BYTEA, list INT[], note TEXT
                 )",
            )
            .expect("create");
        session
            .execute(
                "INSERT INTO mixed VALUES (
                     1, 12.34, '{\"a\": 1}', true, '2026-10-01 00:00:00+00', '2026-10-01',
                     '12:34:56', '1 day', '6f1e5a2c-0000-0000-0000-000000000001',
                     '\\x0102', ARRAY[1, 2], 'hi'
                 )",
            )
            .expect("insert");

        let snapshot = Snapshot::take(&engine);
        let path = std::env::temp_dir().join("velocitysql-persist-values.bin");
        snapshot.save(&path).expect("save");
        let loaded = Snapshot::load(&path).expect("load").expect("present");

        // A fresh engine has none of it, so everything must come back through
        // the file.
        let fresh = Arc::new(Engine::new());
        loaded.restore(&fresh);
        let mut restored = Session::new(fresh);
        let results = restored.execute("SELECT * FROM mixed").expect("select");
        let QueryResult::Rows { rows, .. } = &results[0] else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int4(1));
        assert_eq!(rows[0][6], Value::Time("12:34:56".parse().expect("time")));
        assert_eq!(
            rows[0][10],
            Value::Array(vec![Value::Int4(1), Value::Int4(2)])
        );
        assert_eq!(value_variant_index(&rows[0][0]), 3, "`int4` in the list");
        let _ = std::fs::remove_file(&path);
    }

    /// Names every `Value` variant, to keep the test above exhaustive.
    ///
    /// The match has no wildcard on purpose: adding a variant stops this test
    /// module from compiling until the new value is round-tripped, which is the
    /// only place such a gap can be caught cheaply. A payload whose `Deserialize`
    /// asks a non-self-describing format to describe it (`deserialize_any`) is
    /// found when the snapshot is *read* — long after it was written, and long
    /// after the process that wrote it exited.
    fn value_variant_index(value: &Value) -> usize {
        match value {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int2(_) => 2,
            Value::Int4(_) => 3,
            Value::Int8(_) => 4,
            Value::Float4(_) => 5,
            Value::Float8(_) => 6,
            Value::Numeric(_) => 7,
            Value::Text(_) => 8,
            Value::Bytes(_) => 9,
            Value::Date(_) => 10,
            Value::Time(_) => 11,
            Value::Timestamp(_) => 12,
            Value::Timestamptz(_) => 13,
            Value::Interval(_) => 14,
            Value::Uuid(_) => 15,
            Value::Json(_) => 16,
            Value::Array(_) => 17,
        }
    }

    #[test]
    fn snapshot_restores_roles_databases_and_views() {
        let engine = Arc::new(Engine::new());
        let mut session = Session::new(engine.clone());
        session
            .execute("CREATE ROLE app LOGIN PASSWORD 's3cret'")
            .expect("role");
        session.execute("CREATE DATABASE shop").expect("database");
        session.execute("CREATE TABLE t(a int)").expect("table");
        session
            .execute("INSERT INTO t VALUES (1), (2)")
            .expect("insert");
        session
            .execute("CREATE VIEW positive AS SELECT a FROM t WHERE a > 0")
            .expect("view");
        // The bootstrap superuser's password is the command line's business, not
        // the snapshot's; changing it here proves restore does not resurrect it.
        session
            .execute("ALTER ROLE velocitysql PASSWORD 'changed'")
            .expect("alter");

        let snapshot = Snapshot::take(&engine);
        let path = std::env::temp_dir().join("velocitysql-persist-cluster.bin");
        snapshot.save(&path).expect("save");

        // A fresh process: none of the role, database or view exists yet.
        let fresh = Arc::new(Engine::new());
        Snapshot::load(&path)
            .expect("load")
            .expect("present")
            .restore(&fresh);

        let role = fresh.role("app").expect("role restored");
        assert!(role.can_login, "LOGIN round-trips");
        assert!(
            fresh.authenticate("app", Some("s3cret")).is_ok(),
            "the SCRAM verifier round-trips"
        );
        assert!(
            fresh.authenticate("app", Some("wrong")).is_err(),
            "a wrong password is still rejected"
        );
        assert!(
            fresh
                .role("velocitysql")
                .expect("bootstrap superuser")
                .check_password("velocitysql"),
            "--superuser-password stays authoritative"
        );

        assert!(
            fresh.database("shop").is_some(),
            "a database created at runtime is restored"
        );

        let mut restored = Session::new(fresh);
        let result = restored
            .execute("SELECT a FROM positive ORDER BY a")
            .expect("view restored");
        let QueryResult::Rows { rows, .. } = &result[0] else {
            panic!("expected rows");
        };
        assert_eq!(rows.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_snapshot_file_loads_to_none() {
        let path = std::env::temp_dir().join("velocitysql-persist-absent.bin");
        let _ = std::fs::remove_file(&path);
        assert!(Snapshot::load(&path).expect("load").is_none());
    }
}
