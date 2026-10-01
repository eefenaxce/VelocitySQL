//! The in-memory row store.
//!
//! # Layout
//!
//! A table is a `Vec<Option<RowVersion>>` addressed by [`RowId`] (the slot
//! index), plus a list of secondary indexes over the same identifiers. Slot
//! indices never move, which is what makes them safe to store inside indexes.
//!
//! # MVCC readiness
//!
//! Every slot carries `xmin`/`xmax`, the inserting and deleting transaction
//! ids, and `RowId` is exactly the shape a version chain needs (`next` simply
//! becomes another `RowId`). In milestone M1 there is no transaction manager, so
//! all writers use [`XID_NONE`] and visibility reduces to "slot is occupied";
//! M2 only has to fill those two fields in — no storage layout change, and no
//! index rebuild.
//!
//! # Concurrency
//!
//! One `RwLock` per table guards rows *and* indexes together. Splitting them
//! would allow a reader to observe a row without its index entry, which is a
//! correctness hazard far worse than the contention it would save in M1.
//! Readers take the lock per row rather than for a whole scan, so a scan never
//! blocks writers for longer than a single row copy.
//!
//! The definition is a second lock, and writers always take it second, after the
//! rows. That is what lets [`Table::restructure`] change a relation's shape:
//! `ADD COLUMN` appends a value to every row, `ALTER COLUMN ... TYPE` rewrites
//! them in place, and the two steps happen as one indivisible change, so no
//! reader can catch a row that does not match the layout it is read with.

use std::sync::Arc;

use parking_lot::RwLock;

use vsql_catalog::{IndexDef, IndexMethod, TableDef};
use vsql_types::{Row, Value};

use crate::error::StorageError;
use crate::index::{Index, IndexKey};

/// Physical row identifier: the slot index inside a [`Table`].
pub type RowId = u32;

/// Transaction id.
///
/// Milestone M2 replaces this alias with `vsql_txn::Xid`; the values are already
/// carried inside every row version, so nothing else has to change.
pub type Xid = u64;

/// The transaction id used before a transaction manager exists.
pub const XID_NONE: Xid = 0;

/// One stored row plus its MVCC bookkeeping.
#[derive(Clone, Debug, PartialEq)]
pub struct RowVersion {
    /// Transaction that inserted this version.
    pub xmin: Xid,
    /// Transaction that deleted this version, or [`XID_NONE`] when live.
    pub xmax: Xid,
    /// The payload.
    pub values: Row,
}

impl RowVersion {
    /// Wraps a row with no MVCC information (milestone M1 behaviour).
    pub fn live(values: Row) -> Self {
        RowVersion {
            xmin: XID_NONE,
            xmax: XID_NONE,
            values,
        }
    }

    /// `true` when the version has not been deleted.
    pub fn is_live(&self) -> bool {
        self.xmax == XID_NONE
    }
}

#[derive(Debug, Default)]
struct TableData {
    rows: Vec<Option<RowVersion>>,
    indexes: Vec<Index>,
}

/// An in-memory relation: a row store plus its secondary indexes.
#[derive(Debug)]
pub struct Table {
    /// The current layout. Behind a lock because `ALTER TABLE` replaces it.
    def: RwLock<Arc<TableDef>>,
    data: RwLock<TableData>,
}

impl Table {
    /// Creates an empty table for `def`.
    pub fn new(def: Arc<TableDef>) -> Table {
        Table {
            def: RwLock::new(def),
            data: RwLock::new(TableData::default()),
        }
    }

    /// The table's definition.
    ///
    /// Returned by value so a statement keeps a consistent layout even when a
    /// concurrent `ALTER TABLE` swaps the definition underneath it.
    pub fn def(&self) -> Arc<TableDef> {
        self.def.read().clone()
    }

    /// Number of live rows.
    pub fn len(&self) -> usize {
        let data = self.data.read();
        data.rows.iter().filter(|slot| slot.is_some()).count()
    }

    /// `true` when the table holds no rows.
    pub fn is_empty(&self) -> bool {
        self.data.read().rows.iter().all(Option::is_none)
    }

    // ------------------------------------------------------------- mutations

    /// Inserts a row, enforcing `NOT NULL` and unique constraints.
    ///
    /// Uniqueness is checked against every unique index before anything is
    /// mutated, so a rejected insert leaves the table untouched.
    pub fn insert(&self, row: Row) -> Result<RowId, StorageError> {
        let mut data = self.data.write();
        let def = self.def.read().clone();
        self.check_row(&def, &row)?;
        self.check_unique(&data, &row, None)?;
        let rid = data.rows.len() as RowId;
        for index in data.indexes.iter_mut() {
            index.insert_row(rid, &row);
        }
        data.rows.push(Some(RowVersion::live(row)));
        // The count lives on the definition, so it is bumped while the rows are
        // still locked: an `ALTER TABLE` cannot slip a fresh definition in
        // between and lose the increment.
        def.add_rows(1);
        Ok(rid)
    }

    /// Replaces the row at `rid`.
    ///
    /// Returns `Ok(false)` when the slot is empty, which happens when a
    /// concurrent statement deleted the row after the scan that found it.
    ///
    /// Milestone M1 updates in place; M2 turns this into "close the old version,
    /// append a new one" behind the same signature.
    pub fn update(&self, rid: RowId, row: Row) -> Result<bool, StorageError> {
        let mut data = self.data.write();
        let def = self.def.read().clone();
        self.check_row(&def, &row)?;
        let Some(Some(existing)) = data.rows.get(rid as usize).cloned() else {
            return Ok(false);
        };
        self.check_unique(&data, &row, Some(rid))?;
        for index in data.indexes.iter_mut() {
            index.remove_row(rid, &existing.values);
            index.insert_row(rid, &row);
        }
        data.rows[rid as usize] = Some(RowVersion {
            xmin: existing.xmin,
            xmax: existing.xmax,
            values: row,
        });
        Ok(true)
    }

    /// Deletes the row at `rid`, returning the values it held.
    pub fn delete(&self, rid: RowId) -> Result<Option<Row>, StorageError> {
        let mut data = self.data.write();
        let Some(slot) = data.rows.get_mut(rid as usize) else {
            return Ok(None);
        };
        let Some(existing) = slot.take() else {
            return Ok(None);
        };
        for index in data.indexes.iter_mut() {
            index.remove_row(rid, &existing.values);
        }
        self.def.read().add_rows(-1);
        Ok(Some(existing.values))
    }

    /// Removes every row, keeping the definition and the indexes.
    pub fn truncate(&self) {
        let mut data = self.data.write();
        data.rows.clear();
        for index in data.indexes.iter_mut() {
            let def = index.def().clone();
            *index = Index::new(def);
        }
        self.def.read().set_rows(0);
    }

    /// Installs `def` as the table's layout and rewrites every row with `f`.
    ///
    /// Every row is converted *before* anything is stored, so a conversion that
    /// fails half way through leaves the table exactly as it was. The indexes are
    /// re-derived from the new rows in the same step, because an index key is
    /// built out of column values and a rewritten row invalidates its entries.
    pub fn restructure<E>(
        &self,
        def: Arc<TableDef>,
        mut f: impl FnMut(&Row) -> Result<Row, E>,
    ) -> Result<(), E> {
        let mut data = self.data.write();
        let mut rewritten = Vec::with_capacity(data.rows.len());
        for slot in &data.rows {
            rewritten.push(match slot {
                Some(version) => Some(RowVersion {
                    xmin: version.xmin,
                    xmax: version.xmax,
                    values: f(&version.values)?,
                }),
                None => None,
            });
        }
        data.rows = rewritten;
        let TableData { rows, indexes } = &mut *data;
        for index in indexes.iter_mut() {
            let index_def = index.def().clone();
            let entries = rows.iter().enumerate().filter_map(|(rid, slot)| {
                slot.as_ref().map(|version| (rid as RowId, &version.values))
            });
            *index = Index::build(index_def, entries);
        }
        *self.def.write() = def;
        Ok(())
    }

    /// Installs `def` as the table's layout, leaving the rows untouched.
    ///
    /// Only for a change that does not move data - a rename, a new default, a
    /// nullability flag. Nothing here verifies that the stored rows still match
    /// the layout, so a change to the columns' *shape* must go through
    /// [`Table::restructure`] instead.
    pub fn set_def(&self, def: Arc<TableDef>) {
        let _rows = self.data.read();
        *self.def.write() = def;
    }

    // -------------------------------------------------------------- readers

    /// Clones the row at `rid`.
    pub fn row(&self, rid: RowId) -> Option<Row> {
        let data = self.data.read();
        data.rows
            .get(rid as usize)
            .and_then(|slot| slot.as_ref())
            .map(|version| version.values.clone())
    }

    /// Borrows the row at `rid` for the duration of `f`.
    pub fn with_row<R>(&self, rid: RowId, f: impl FnOnce(&Row) -> R) -> Option<R> {
        let data = self.data.read();
        data.rows
            .get(rid as usize)
            .and_then(|slot| slot.as_ref())
            .map(|version| f(&version.values))
    }

    /// Snapshot of every live [`RowId`], in insertion order.
    pub fn rids(&self) -> Vec<RowId> {
        let data = self.data.read();
        data.rows
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|_| index as RowId))
            .collect()
    }

    /// Visits every live row, stopping early when `f` returns `false`.
    ///
    /// The read lock is held for the whole traversal, so this is the right tool
    /// for bulk work (aggregation, WAL snapshotting) but not for long-running
    /// client-visible loops.
    pub fn for_each(&self, mut f: impl FnMut(RowId, &Row) -> bool) {
        let data = self.data.read();
        for (index, slot) in data.rows.iter().enumerate() {
            if let Some(version) = slot {
                if !f(index as RowId, &version.values) {
                    return;
                }
            }
        }
    }

    /// Clones every live row. Used by operators that must materialise their
    /// input (sort, aggregate, hash join build side).
    pub fn snapshot(&self) -> Vec<(RowId, Row)> {
        let data = self.data.read();
        data.rows
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.as_ref()
                    .map(|version| (index as RowId, version.values.clone()))
            })
            .collect()
    }

    /// Exact number of physical slots, including dead ones.
    pub fn slot_count(&self) -> usize {
        self.data.read().rows.len()
    }

    // -------------------------------------------------------------- indexes

    /// Attaches an index, backfilling it from the current rows.
    pub fn attach_index(&self, def: Arc<IndexDef>) {
        let mut data = self.data.write();
        if data.indexes.iter().any(|i| i.def().oid == def.oid) {
            return;
        }
        let built = {
            let rows = data.rows.iter().enumerate().filter_map(|(index, slot)| {
                slot.as_ref()
                    .map(|version| (index as RowId, &version.values))
            });
            Index::build(def, rows)
        };
        data.indexes.push(built);
    }

    /// Removes an index.
    pub fn detach_index(&self, oid: u32) {
        self.data
            .write()
            .indexes
            .retain(|index| index.def().oid != oid);
    }

    /// The indexes attached to this table.
    pub fn indexes(&self) -> Vec<Arc<IndexDef>> {
        self.data
            .read()
            .indexes
            .iter()
            .map(|index| index.def().clone())
            .collect()
    }

    /// Equality probe against an index.
    pub fn index_lookup(&self, index_oid: u32, key: &[Value]) -> Option<Vec<RowId>> {
        let key = IndexKey::from_values(key.to_vec())?;
        let data = self.data.read();
        let index = data.indexes.iter().find(|i| i.def().oid == index_oid)?;
        Some(index.lookup(&key))
    }

    /// Range probe on the leading index column.
    pub fn index_range(
        &self,
        index_oid: u32,
        low: Option<&Value>,
        high: Option<&Value>,
    ) -> Option<Vec<RowId>> {
        let data = self.data.read();
        let index = data.indexes.iter().find(|i| i.def().oid == index_oid)?;
        index.range_first(low, high)
    }

    /// Total number of index entries, useful for `EXPLAIN` and tests.
    pub fn index_key_count(&self, index_oid: u32) -> usize {
        let data = self.data.read();
        data.indexes
            .iter()
            .find(|i| i.def().oid == index_oid)
            .map(|index| index.key_count())
            .unwrap_or(0)
    }

    /// Creates an index definition backed by the given columns.
    pub fn index_method(&self, index_oid: u32) -> Option<IndexMethod> {
        let data = self.data.read();
        data.indexes
            .iter()
            .find(|i| i.def().oid == index_oid)
            .map(|index| index.def().method)
    }

    // ----------------------------------------------------------- validation

    fn check_row(&self, def: &TableDef, row: &Row) -> Result<(), StorageError> {
        if row.len() != def.column_count() {
            return Err(StorageError::ArityMismatch {
                table: def.name.clone(),
                expected: def.column_count(),
                actual: row.len(),
            });
        }
        for column in &def.columns {
            if column.nullable || column.dropped {
                continue;
            }
            if row[column.ordinal].is_null() {
                return Err(StorageError::NotNullViolation {
                    table: def.name.clone(),
                    column: column.name.clone(),
                });
            }
        }
        Ok(())
    }

    fn check_unique(
        &self,
        data: &TableData,
        row: &Row,
        ignore: Option<RowId>,
    ) -> Result<(), StorageError> {
        for index in &data.indexes {
            if !index.is_unique() {
                continue;
            }
            let def = index.def().clone();
            let Some(key) = IndexKey::extract(&def, row) else {
                continue;
            };
            if index.holds_other_than(&key, ignore) {
                return Err(StorageError::UniqueViolation {
                    index: def.name.clone(),
                });
            }
        }
        Ok(())
    }
}
