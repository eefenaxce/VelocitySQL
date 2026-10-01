//! The catalog itself: a namespace of schemas, tables, indexes and sequences.
//!
//! # Concurrency
//!
//! The catalog is read on every statement (name resolution) but written rarely
//! (DDL only). A single `RwLock` therefore beats a sharded map: the read path
//! takes a shared lock, while DDL serialises. Definitions are handed out as
//! `Arc<TableDef>` so a statement keeps working with a consistent snapshot even
//! if a concurrent `DROP TABLE` removes the entry from the catalog.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::defs::{
    CheckDef, ColumnDef, ForeignKeyDef, IndexDef, IndexMethod, SchemaDef, SequenceDef, TableDef,
};
use crate::error::CatalogError;
use crate::oid::{oids, Oid};

#[derive(Debug, Default)]
struct Inner {
    /// Schemas are a handful of objects, so a vector beats a hash map here.
    schemas: Vec<SchemaDef>,
    tables: HashMap<Oid, Arc<TableDef>>,
    table_by_name: HashMap<(Oid, String), Oid>,
    indexes: HashMap<Oid, Arc<IndexDef>>,
    index_by_name: HashMap<(Oid, String), Oid>,
    sequences: HashMap<(Oid, String), Arc<SequenceDef>>,
}

/// The VelocitySQL catalog.
///
/// Every relation the engine can address lives here. The storage engine holds
/// the rows; the catalog holds the identity and layout.
#[derive(Debug)]
pub struct Catalog {
    inner: RwLock<Inner>,
    next_oid: AtomicU32,
}

impl Default for Catalog {
    fn default() -> Self {
        Catalog::new()
    }
}

impl Catalog {
    /// Creates a catalog pre-populated with PostgreSQL's default schemas.
    pub fn new() -> Self {
        let inner = Inner {
            schemas: vec![
                SchemaDef {
                    oid: oids::PG_CATALOG_NAMESPACE,
                    name: "pg_catalog".to_string(),
                    is_system: true,
                },
                SchemaDef {
                    oid: oids::PUBLIC_NAMESPACE,
                    name: "public".to_string(),
                    is_system: false,
                },
                SchemaDef {
                    oid: oids::INFORMATION_SCHEMA_NAMESPACE,
                    name: "information_schema".to_string(),
                    is_system: true,
                },
            ],
            ..Inner::default()
        };
        Catalog {
            inner: RwLock::new(inner),
            next_oid: AtomicU32::new(oids::FIRST_USER_OID),
        }
    }

    /// Allocates a fresh OID.
    ///
    /// OIDs are never reused, exactly like PostgreSQL, which keeps them usable
    /// as stable identities for the lifetime of a session.
    pub fn next_oid(&self) -> Oid {
        self.next_oid.fetch_add(1, Ordering::SeqCst)
    }

    /// Raises the OID counter so no future allocation collides with `at_least`.
    ///
    /// Used when a snapshot restores objects that carried their own OIDs: the
    /// next `CREATE` must allocate past everything already on disk.
    pub fn bump_next_oid(&self, at_least: Oid) {
        let _ = self
            .next_oid
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                if at_least > current {
                    Some(at_least)
                } else {
                    None
                }
            });
    }

    /// Looks up a schema's name by OID.
    pub fn schema_name(&self, oid: Oid) -> Option<String> {
        let inner = self.inner.read();
        inner
            .schemas
            .iter()
            .find(|schema| schema.oid == oid)
            .map(|schema| schema.name.clone())
    }

    // ------------------------------------------------------------- schemas

    /// Creates a schema, returning the existing one when `if_not_exists`.
    pub fn create_schema(
        &self,
        name: &str,
        if_not_exists: bool,
    ) -> Result<SchemaDef, CatalogError> {
        let name = normalise(name);
        let mut inner = self.inner.write();
        if let Some(existing) = inner.schemas.iter().find(|s| s.name == name) {
            if if_not_exists {
                return Ok(existing.clone());
            }
            return Err(CatalogError::SchemaExists(name));
        }
        let schema = SchemaDef {
            oid: self.next_oid(),
            name,
            is_system: false,
        };
        inner.schemas.push(schema.clone());
        Ok(schema)
    }

    /// Drops a schema. Refuses to drop system schemas or non-empty schemas.
    pub fn drop_schema(&self, name: &str, if_exists: bool) -> Result<(), CatalogError> {
        let name = normalise(name);
        let mut inner = self.inner.write();
        let Some(position) = inner.schemas.iter().position(|s| s.name == name) else {
            return if if_exists {
                Ok(())
            } else {
                Err(CatalogError::UndefinedSchema(name))
            };
        };
        if inner.schemas[position].is_system {
            return Err(CatalogError::Other(format!(
                "cannot drop schema \"{name}\" because it is a system schema"
            )));
        }
        let oid = inner.schemas[position].oid;
        if inner.table_by_name.keys().any(|(schema, _)| *schema == oid) {
            return Err(CatalogError::Other(format!(
                "cannot drop schema \"{name}\" because other objects depend on it"
            )));
        }
        inner.schemas.remove(position);
        Ok(())
    }

    /// Looks up a schema by name.
    pub fn schema(&self, name: &str) -> Option<SchemaDef> {
        let name = normalise(name);
        self.inner
            .read()
            .schemas
            .iter()
            .find(|s| s.name == name)
            .cloned()
    }

    /// Resolves a schema by name, erroring when it does not exist.
    pub fn resolve_schema(&self, name: &str) -> Result<SchemaDef, CatalogError> {
        self.schema(name)
            .ok_or_else(|| CatalogError::UndefinedSchema(name.to_string()))
    }

    /// All schemas, in creation order.
    pub fn schemas(&self) -> Vec<SchemaDef> {
        self.inner.read().schemas.clone()
    }

    // -------------------------------------------------------------- tables

    /// Creates a table in `schema_oid`.
    pub fn create_table(
        &self,
        schema_oid: Oid,
        name: &str,
        columns: Vec<ColumnDef>,
        primary_key: Vec<usize>,
        if_not_exists: bool,
        is_system: bool,
    ) -> Result<Arc<TableDef>, CatalogError> {
        let name = normalise(name);
        for (index, column) in columns.iter().enumerate() {
            if columns[..index]
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(&column.name))
            {
                return Err(CatalogError::DuplicateColumn(column.name.clone()));
            }
        }
        let mut inner = self.inner.write();
        if let Some(existing) = inner.table_by_name.get(&(schema_oid, name.clone())) {
            if if_not_exists {
                return Ok(inner.tables[existing].clone());
            }
            return Err(CatalogError::RelationExists(name));
        }
        let mut def = TableDef::new(self.next_oid(), schema_oid, name.clone(), columns);
        def.primary_key = primary_key;
        def.is_system = is_system;
        let def = Arc::new(def);
        inner.tables.insert(def.oid, def.clone());
        inner.table_by_name.insert((schema_oid, name), def.oid);
        Ok(def)
    }

    /// Installs a new definition for an existing table (`ALTER TABLE`).
    ///
    /// The OID is the table's identity, not the definition's: the index list and
    /// the live row count belong to the table, so they are carried over from the
    /// definition being replaced and a caller only has to describe the layout.
    /// Renaming moves the name index entry, and a name already taken by another
    /// relation is refused.
    pub fn replace_table(&self, def: Arc<TableDef>) -> Result<Arc<TableDef>, CatalogError> {
        let mut inner = self.inner.write();
        let Some(existing) = inner.tables.get(&def.oid).cloned() else {
            return Err(CatalogError::UndefinedTable(def.name.clone()));
        };
        if existing.name != def.name {
            let taken = inner
                .table_by_name
                .get(&(def.schema_oid, def.name.clone()))
                .copied();
            if taken.is_some_and(|oid| oid != def.oid) {
                return Err(CatalogError::RelationExists(def.name.clone()));
            }
            inner
                .table_by_name
                .remove(&(existing.schema_oid, existing.name.clone()));
        }
        inner
            .table_by_name
            .insert((def.schema_oid, def.name.clone()), def.oid);
        let installed = def.index_oids();
        for oid in existing.index_oids() {
            if !installed.contains(&oid) {
                def.add_index(oid);
            }
        }
        def.set_rows(existing.rows());
        inner.tables.insert(def.oid, def.clone());
        Ok(def)
    }

    /// Installs a fully-built table definition under its own OID.
    ///
    /// Unlike [`Catalog::create_table`], which mints a fresh OID, this preserves
    /// the OID a restored snapshot carried, so foreign keys and indexes that
    /// point at it keep resolving. Returns the existing definition when a table
    /// of the same name is already present, making restore idempotent against a
    /// schema already rebuilt by `--init`/`--demo`.
    pub fn install_table(&self, def: Arc<TableDef>) -> Result<Arc<TableDef>, CatalogError> {
        let mut inner = self.inner.write();
        if let Some(existing_oid) = inner.table_by_name.get(&(def.schema_oid, def.name.clone())) {
            return Ok(inner.tables[existing_oid].clone());
        }
        inner.tables.insert(def.oid, def.clone());
        inner
            .table_by_name
            .insert((def.schema_oid, def.name.clone()), def.oid);
        Ok(def)
    }

    /// Installs a fully-built sequence definition under its own OID.
    ///
    /// Mirrors [`Catalog::install_table`] for sequences restored from a snapshot.
    pub fn install_sequence(
        &self,
        def: Arc<SequenceDef>,
    ) -> Result<Arc<SequenceDef>, CatalogError> {
        let mut inner = self.inner.write();
        inner
            .sequences
            .insert((def.schema_oid, def.name.clone()), def.clone());
        Ok(def)
    }

    /// Attaches `CHECK` constraint definitions to a table.
    pub fn set_checks(
        &self,
        oid: Oid,
        checks: Vec<CheckDef>,
    ) -> Result<Arc<TableDef>, CatalogError> {
        self.mutate_table(oid, |def| def.checks = checks)
    }

    /// Attaches a `FOREIGN KEY` constraint to a table.
    pub fn attach_foreign_key(
        &self,
        oid: Oid,
        key: ForeignKeyDef,
    ) -> Result<Arc<TableDef>, CatalogError> {
        self.mutate_table(oid, |def| def.foreign_keys.push(key))
    }

    /// Edits a table's definition in place.
    ///
    /// A copy of the definition (with its identity, index list and row count
    /// carried over) is handed to `f`, then installed through
    /// [`Catalog::replace_table`], which is what keeps the name index, the
    /// storage engine's mount and the `Arc<TableDef>` handed to running
    /// statements consistent.
    fn mutate_table(
        &self,
        oid: Oid,
        f: impl FnOnce(&mut TableDef),
    ) -> Result<Arc<TableDef>, CatalogError> {
        let existing = {
            let inner = self.inner.read();
            inner
                .tables
                .get(&oid)
                .cloned()
                .ok_or(CatalogError::UndefinedTable(format!("oid {oid}")))?
        };
        let mut def = TableDef::new(
            existing.oid,
            existing.schema_oid,
            existing.name.clone(),
            existing.columns.clone(),
        );
        def.primary_key = existing.primary_key.clone();
        def.is_system = existing.is_system;
        def.owner = existing.owner.clone();
        def.checks = existing.checks.clone();
        def.foreign_keys = existing.foreign_keys.clone();
        def.set_rows(existing.rows());
        for index_oid in existing.index_oids() {
            def.add_index(index_oid);
        }
        f(&mut def);
        self.replace_table(Arc::new(def))
    }

    /// Drops a table.
    pub fn drop_table(
        &self,
        schema_oid: Oid,
        name: &str,
        if_exists: bool,
    ) -> Result<Option<Arc<TableDef>>, CatalogError> {
        let name = normalise(name);
        let mut inner = self.inner.write();
        let Some(oid) = inner.table_by_name.remove(&(schema_oid, name.clone())) else {
            return if if_exists {
                Ok(None)
            } else {
                Err(CatalogError::UndefinedTable(name))
            };
        };
        let def = inner
            .tables
            .remove(&oid)
            .expect("table_by_name and tables must stay in sync");
        for index_oid in def.index_oids() {
            if let Some(index) = inner.indexes.remove(&index_oid) {
                inner
                    .index_by_name
                    .remove(&(schema_oid, index.name.clone()));
            }
        }
        Ok(Some(def))
    }

    /// Looks up a table by OID.
    pub fn table(&self, oid: Oid) -> Option<Arc<TableDef>> {
        self.inner.read().tables.get(&oid).cloned()
    }

    /// Looks up a table by schema and name.
    pub fn table_named(&self, schema_oid: Oid, name: &str) -> Option<Arc<TableDef>> {
        let name = normalise(name);
        let inner = self.inner.read();
        inner
            .table_by_name
            .get(&(schema_oid, name))
            .and_then(|oid| inner.tables.get(oid))
            .cloned()
    }

    /// Resolves a (possibly qualified) relation name.
    ///
    /// A single-part name is searched in `public` first and then in
    /// `pg_catalog`, mirroring PostgreSQL's default `search_path`. Multi-part
    /// names use the last two components as `schema.relation`, so
    /// `db.schema.table` works the way clients emit it.
    pub fn resolve_table(&self, name: &[String]) -> Result<Arc<TableDef>, CatalogError> {
        let parts: Vec<String> = name.iter().map(|p| normalise(p)).collect();
        let (schema_name, table_name) = match parts.as_slice() {
            [table] => (None, table.clone()),
            [schema, table, ..] => (Some(schema.clone()), table.clone()),
            [] => return Err(CatalogError::Other("empty relation name".into())),
        };
        match schema_name {
            Some(schema) => {
                let schema = self.resolve_schema(&schema)?;
                self.table_named(schema.oid, &table_name)
                    .ok_or(CatalogError::UndefinedTable(table_name))
            }
            None => {
                for candidate in ["public", "pg_catalog"] {
                    if let Some(schema) = self.schema(candidate) {
                        if let Some(table) = self.table_named(schema.oid, &table_name) {
                            return Ok(table);
                        }
                    }
                }
                Err(CatalogError::UndefinedTable(table_name))
            }
        }
    }

    /// All tables, ordered by OID for stable output.
    pub fn tables(&self) -> Vec<Arc<TableDef>> {
        let mut tables: Vec<Arc<TableDef>> = self.inner.read().tables.values().cloned().collect();
        tables.sort_by_key(|t| t.oid);
        tables
    }

    // ------------------------------------------------------------- indexes

    /// Registers an index and attaches it to its table.
    pub fn register_index(&self, def: IndexDef) -> Result<Arc<IndexDef>, CatalogError> {
        let mut inner = self.inner.write();
        let table = inner
            .tables
            .get(&def.table_oid)
            .cloned()
            .ok_or_else(|| CatalogError::UndefinedTable(def.table_oid.to_string()))?;
        let key = (def.table_oid, def.name.clone());
        if let Some(existing) = inner.index_by_name.get(&key) {
            return Ok(inner.indexes[existing].clone());
        }
        let def = Arc::new(def);
        table.add_index(def.oid);
        inner.index_by_name.insert(key, def.oid);
        inner.indexes.insert(def.oid, def.clone());
        Ok(def)
    }

    /// Removes an index from the catalog and detaches it from its table.
    ///
    /// Returns the definition that was removed, or `None` when no index has that
    /// OID; dropping an already-dropped index is not an error.
    pub fn drop_index(&self, oid: Oid) -> Option<Arc<IndexDef>> {
        let mut inner = self.inner.write();
        let def = inner.indexes.remove(&oid)?;
        inner
            .index_by_name
            .remove(&(def.table_oid, def.name.clone()));
        if let Some(table) = inner.tables.get(&def.table_oid) {
            table.remove_index(oid);
        }
        Some(def)
    }

    /// Looks up an index by name within a schema.
    pub fn index_named(&self, schema_oid: Oid, name: &str) -> Option<Arc<IndexDef>> {
        let name = normalise(name);
        let inner = self.inner.read();
        inner
            .indexes
            .values()
            .find(|index| {
                index.name == name
                    && inner
                        .tables
                        .get(&index.table_oid)
                        .map(|table| table.schema_oid == schema_oid)
                        .unwrap_or(false)
            })
            .cloned()
    }

    /// All indexes of a table.
    pub fn table_indexes(&self, table_oid: Oid) -> Vec<Arc<IndexDef>> {
        let inner = self.inner.read();
        let Some(table) = inner.tables.get(&table_oid) else {
            return Vec::new();
        };
        table
            .index_oids()
            .into_iter()
            .filter_map(|oid| inner.indexes.get(&oid).cloned())
            .collect()
    }

    /// Finds an index on `table_oid` whose leading columns match `columns`.
    ///
    /// This is the hook the planner uses to turn a `WHERE pk = ?` predicate into
    /// an index scan.
    pub fn index_for_columns(&self, table_oid: Oid, columns: &[usize]) -> Option<Arc<IndexDef>> {
        let inner = self.inner.read();
        let table = inner.tables.get(&table_oid)?;
        table
            .index_oids()
            .into_iter()
            .filter_map(|oid| inner.indexes.get(&oid))
            .find(|index| {
                !columns.is_empty()
                    && index.columns.len() >= columns.len()
                    && index.columns[..columns.len()] == *columns
            })
            .cloned()
    }

    // ----------------------------------------------------------- sequences

    /// Creates a sequence.
    pub fn create_sequence(
        &self,
        schema_oid: Oid,
        name: &str,
        start: i64,
        increment: i64,
        if_not_exists: bool,
    ) -> Result<Arc<SequenceDef>, CatalogError> {
        let name = normalise(name);
        let mut inner = self.inner.write();
        if let Some(existing) = inner.sequences.get(&(schema_oid, name.clone())) {
            if if_not_exists {
                return Ok(existing.clone());
            }
            return Err(CatalogError::RelationExists(name));
        }
        let def = Arc::new(SequenceDef::new(
            self.next_oid(),
            schema_oid,
            name.clone(),
            start,
            increment,
        ));
        inner.sequences.insert((schema_oid, name), def.clone());
        Ok(def)
    }

    /// Advances a sequence, if it exists.
    pub fn nextval(&self, schema_oid: Oid, name: &str) -> Option<i64> {
        let name = normalise(name);
        let inner = self.inner.read();
        inner
            .sequences
            .get(&(schema_oid, name))
            .map(|sequence| sequence.nextval())
    }

    /// Every sequence, sorted by name for deterministic catalog output.
    pub fn sequences(&self) -> Vec<Arc<SequenceDef>> {
        let inner = self.inner.read();
        let mut sequences: Vec<Arc<SequenceDef>> = inner.sequences.values().cloned().collect();
        sequences.sort_by(|left, right| left.name.cmp(&right.name));
        sequences
    }
}

/// SQL identifiers are case-insensitive unless quoted, and the parser already
/// folded unquoted ones; normalising again keeps programmatic API users honest.
fn normalise(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// Convenience re-export so callers can build a simple B-tree index definition.
pub fn btree_index(
    oid: Oid,
    table_oid: Oid,
    name: impl Into<String>,
    columns: Vec<usize>,
) -> IndexDef {
    IndexDef::new(oid, table_oid, name, columns, IndexMethod::BTree)
}
