//! In-memory secondary indexes.
//!
//! Two access methods are implemented, matching the two the planner knows how
//! to exploit:
//!
//! * **B-tree** (`BTreeMap`) — ordered, so it answers `=`, `<`, `>`, `BETWEEN`
//!   and prefix `ORDER BY` without a sort.
//! * **Hash** (`HashMap`) — equality only, but with O(1) probes and no key
//!   comparison beyond hashing.
//!
//! Both store the *physical* row identifiers (`RowId`) rather than rows, so an
//! index lookup never copies table data. A key is the tuple of indexed columns;
//! `IndexKey` is an ordinary value tuple, which means the ordering semantics are
//! exactly [`vsql_types::Value`]'s, with no separate comparator to keep in sync.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::Arc;

use vsql_catalog::{IndexDef, IndexMethod};
use vsql_types::{Row, Value};

use crate::table::RowId;

/// A composite index key.
///
/// Wrapper instead of `Vec<Value>` so the `Ord`/`Hash`/`Eq` implementations are
/// explicit and reviewable: index correctness depends on them agreeing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct IndexKey(pub Vec<Value>);

impl IndexKey {
    /// Extracts the key for `index` from `row`.
    ///
    /// Returns `None` when any indexed column is `NULL`. PostgreSQL treats
    /// `NULL`s as *not equal to anything*, so unique indexes must not collide on
    /// them; by skipping such rows entirely we get that behaviour for free.
    pub fn extract(index: &IndexDef, row: &Row) -> Option<IndexKey> {
        let mut values = Vec::with_capacity(index.columns.len());
        for column in &index.columns {
            let value = row.get(*column)?;
            if value.is_null() {
                return None;
            }
            values.push(value.clone());
        }
        Some(IndexKey(values))
    }

    /// Extracts a key from explicit values (used by index-scan probes).
    pub fn from_values(values: Vec<Value>) -> Option<IndexKey> {
        if values.iter().any(Value::is_null) {
            return None;
        }
        Some(IndexKey(values))
    }

    /// Borrows the key components.
    pub fn values(&self) -> &[Value] {
        &self.0
    }
}

/// A B-tree index.
#[derive(Debug)]
pub struct BTreeIndex {
    /// Index metadata.
    pub def: Arc<IndexDef>,
    /// Key to row identifiers. The vector holds every row with an equal key.
    pub map: BTreeMap<IndexKey, Vec<RowId>>,
}

/// A hash index.
#[derive(Debug)]
pub struct HashIndex {
    /// Index metadata.
    pub def: Arc<IndexDef>,
    /// Key to row identifiers.
    pub map: HashMap<IndexKey, Vec<RowId>>,
}

/// An index on a table.
#[derive(Debug)]
pub enum Index {
    /// Ordered index.
    BTree(BTreeIndex),
    /// Equality-only index.
    Hash(HashIndex),
}

impl Index {
    /// Builds an empty index of the access method described by `def`.
    pub fn new(def: Arc<IndexDef>) -> Index {
        match def.method {
            IndexMethod::BTree => Index::BTree(BTreeIndex {
                def,
                map: BTreeMap::new(),
            }),
            IndexMethod::Hash => Index::Hash(HashIndex {
                def,
                map: HashMap::new(),
            }),
        }
    }

    /// Builds an index and populates it from an iterator of rows.
    ///
    /// Used by `CREATE INDEX`: the table is already populated, so the index has
    /// to be backfilled in one pass.
    pub fn build<'a>(
        def: Arc<IndexDef>,
        rows: impl IntoIterator<Item = (RowId, &'a Row)>,
    ) -> Index {
        let mut index = Index::new(def);
        for (rid, row) in rows {
            index.insert_row(rid, row);
        }
        index
    }

    /// Index metadata.
    pub fn def(&self) -> &Arc<IndexDef> {
        match self {
            Index::BTree(index) => &index.def,
            Index::Hash(index) => &index.def,
        }
    }

    /// `true` when the index enforces uniqueness.
    pub fn is_unique(&self) -> bool {
        self.def().unique
    }

    /// Number of distinct keys.
    pub fn key_count(&self) -> usize {
        match self {
            Index::BTree(index) => index.map.len(),
            Index::Hash(index) => index.map.len(),
        }
    }

    /// Inserts a pre-computed key.
    pub fn insert(&mut self, key: IndexKey, rid: RowId) {
        match self {
            Index::BTree(index) => index.map.entry(key).or_default().push(rid),
            Index::Hash(index) => index.map.entry(key).or_default().push(rid),
        }
    }

    /// Inserts a row, deriving the key from the index definition.
    pub fn insert_row(&mut self, rid: RowId, row: &Row) {
        let def = self.def().clone();
        if let Some(key) = IndexKey::extract(&def, row) {
            self.insert(key, rid);
        }
    }

    /// Removes `rid` from the entry for `key`.
    pub fn remove(&mut self, key: &IndexKey, rid: RowId) {
        let empty = match self {
            Index::BTree(index) => {
                if let Some(entries) = index.map.get_mut(key) {
                    entries.retain(|existing| *existing != rid);
                    entries.is_empty()
                } else {
                    false
                }
            }
            Index::Hash(index) => {
                if let Some(entries) = index.map.get_mut(key) {
                    entries.retain(|existing| *existing != rid);
                    entries.is_empty()
                } else {
                    false
                }
            }
        };
        if empty {
            match self {
                Index::BTree(index) => {
                    index.map.remove(key);
                }
                Index::Hash(index) => {
                    index.map.remove(key);
                }
            }
        }
    }

    /// Removes a row, deriving the key from the index definition.
    pub fn remove_row(&mut self, rid: RowId, row: &Row) {
        let def = self.def().clone();
        if let Some(key) = IndexKey::extract(&def, row) {
            self.remove(&key, rid);
        }
    }

    /// Returns the row identifiers stored under `key`.
    pub fn lookup(&self, key: &IndexKey) -> Vec<RowId> {
        match self {
            Index::BTree(index) => index.map.get(key).cloned().unwrap_or_default(),
            Index::Hash(index) => index.map.get(key).cloned().unwrap_or_default(),
        }
    }

    /// `true` when `key` holds a row other than `ignore`.
    ///
    /// This is the question unique enforcement asks on every insert and update.
    /// It answers without copying the entry list, which [`Index::lookup`] would.
    pub fn holds_other_than(&self, key: &IndexKey, ignore: Option<RowId>) -> bool {
        self.entries(key)
            .is_some_and(|rids| rids.iter().any(|rid| Some(*rid) != ignore))
    }

    /// The rows stored under `key`, borrowed.
    fn entries(&self, key: &IndexKey) -> Option<&[RowId]> {
        match self {
            Index::BTree(index) => index.map.get(key).map(Vec::as_slice),
            Index::Hash(index) => index.map.get(key).map(Vec::as_slice),
        }
    }

    /// Range scan over the *first* key component, inclusive on both ends.
    ///
    /// Only B-tree indexes can answer this; hash indexes return `None`.
    pub fn range_first(&self, low: Option<&Value>, high: Option<&Value>) -> Option<Vec<RowId>> {
        let Index::BTree(index) = self else {
            return None;
        };
        let mut out = Vec::new();
        let start = match low {
            Some(value) => Bound::Included(IndexKey(vec![value.clone()])),
            None => Bound::Unbounded,
        };
        // Every key sharing a first component is contiguous in a B-tree and the
        // shortest tuple sorts first, so seeking to `[low]` and breaking past
        // `high` costs the size of the matching range, not of the index.
        for (key, rids) in index.map.range((start, Bound::Unbounded)) {
            let Some(first) = key.values().first() else {
                continue;
            };
            if let Some(low) = low {
                if first < low {
                    continue;
                }
            }
            if let Some(high) = high {
                if first > high {
                    continue;
                }
            }
            out.extend_from_slice(rids);
        }
        Some(out)
    }

    /// Full scan in key order (B-tree) or unspecified order (hash).
    pub fn scan_all(&self) -> Vec<RowId> {
        match self {
            Index::BTree(index) => index.map.values().flatten().copied().collect(),
            Index::Hash(index) => index.map.values().flatten().copied().collect(),
        }
    }
}
