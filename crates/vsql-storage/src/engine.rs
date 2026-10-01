//! The storage engine: the set of live tables.
//!
//! The engine deliberately knows nothing about names. It keys tables by OID and
//! is told about their layout by the catalog, which keeps resolution logic in one
//! place and lets a table survive a rename without being rebuilt.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use vsql_catalog::{Oid, TableDef};

use crate::table::Table;

/// Holds every table currently mounted in memory.
#[derive(Debug, Default)]
pub struct Storage {
    tables: RwLock<HashMap<Oid, Arc<Table>>>,
}

impl Storage {
    /// Creates an empty storage engine.
    pub fn new() -> Self {
        Storage::default()
    }

    /// Mounts a table for `def`.
    ///
    /// Mounting the same definition twice returns a fresh, empty table, exactly
    /// like `CREATE TABLE` on a name the catalog had already forgotten.
    pub fn create_table(&self, def: Arc<TableDef>) -> Arc<Table> {
        let table = Arc::new(Table::new(def.clone()));
        self.tables.write().insert(def.oid, table.clone());
        table
    }

    /// Unmounts a table.
    pub fn drop_table(&self, oid: Oid) -> Option<Arc<Table>> {
        self.tables.write().remove(&oid)
    }

    /// Looks up a table by OID.
    pub fn table(&self, oid: Oid) -> Option<Arc<Table>> {
        self.tables.read().get(&oid).cloned()
    }

    /// Every mounted table, ordered by OID for deterministic output.
    pub fn tables(&self) -> Vec<Arc<Table>> {
        let mut tables: Vec<Arc<Table>> = self.tables.read().values().cloned().collect();
        tables.sort_by_key(|table| table.def().oid);
        tables
    }

    /// Number of mounted tables.
    pub fn len(&self) -> usize {
        self.tables.read().len()
    }

    /// `true` when nothing is mounted.
    pub fn is_empty(&self) -> bool {
        self.tables.read().is_empty()
    }

    /// Drops every table. Used by tests and by `TRUNCATE`-style resets.
    pub fn clear(&self) {
        self.tables.write().clear();
    }
}
