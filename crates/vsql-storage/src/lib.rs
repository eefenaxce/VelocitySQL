//! VelocitySQL's in-memory storage engine.
//!
//! # Responsibilities
//!
//! * Own the rows (`Table`) and the secondary indexes over them (`Index`).
//! * Enforce the constraints that are checkable from a single row: `NOT NULL`
//!   and unique/primary-key indexes.
//! * Hand out stable [`RowId`]s so indexes and the MVCC version chain can refer
//!   to a row without copying it.
//!
//! # What it deliberately does *not* do
//!
//! * **Name resolution.** The catalog owns names; the engine is keyed by OID.
//! * **Type coercion.** The executor converts literals before calling `insert`,
//!   so the engine never has to guess a target type.
//! * **Durability.** The WAL (`vsql-wal`, milestone M2) records the *logical*
//!   row operations the executor performs; the engine stays a pure data
//!   structure, which is what keeps it fast and testable.
//!
//! # Locking model
//!
//! Each table owns an `RwLock` covering rows and indexes together; the engine
//! owns an `RwLock` over the table registry. Nothing nests beyond that, so there
//! is no lock-ordering hazard to reason about.
//!
//! ```
//! use std::sync::Arc;
//! use vsql_catalog::{btree_index, Catalog, ColumnDef};
//! use vsql_storage::{Storage, StorageError};
//! use vsql_types::{DataType, Value};
//!
//! let catalog = Catalog::new();
//! let public = catalog.resolve_schema("public").unwrap();
//! let def = catalog
//!     .create_table(
//!         public.oid,
//!         "users",
//!         vec![
//!             ColumnDef::new("id", DataType::Int4, 0).not_null(),
//!             ColumnDef::new("email", DataType::Text, 1),
//!         ],
//!         vec![0],
//!         false,
//!         false,
//!     )
//!     .unwrap();
//! let storage = Storage::new();
//! let table = storage.create_table(def.clone());
//! for oid in [table.def().oid] {
//!     let index = btree_index(catalog.next_oid(), oid, "users_pkey", vec![0]).unique();
//!     table.attach_index(Arc::new(index));
//! }
//!
//! table.insert(vec![Value::Int4(1), Value::Text("a@example.com".into())]).unwrap();
//! let error = table
//!     .insert(vec![Value::Int4(1), Value::Text("b@example.com".into())])
//!     .unwrap_err();
//! assert!(matches!(error, StorageError::UniqueViolation { .. }));
//! assert_eq!(table.len(), 1);
//! ```
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod engine;
pub mod error;
pub mod index;
pub mod table;

pub use engine::Storage;
pub use error::StorageError;
pub use index::{BTreeIndex, HashIndex, Index, IndexKey};
pub use table::{RowId, RowVersion, Table, Xid, XID_NONE};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use vsql_catalog::{btree_index, Catalog, ColumnDef, IndexMethod};
    use vsql_types::{DataType, Row, Value};

    fn table(columns: usize, pk: Option<Vec<usize>>) -> (Catalog, Arc<Table>) {
        let catalog = Catalog::new();
        let public = catalog.resolve_schema("public").unwrap();
        let defs = (0..columns)
            .map(|i| ColumnDef::new(format!("c{i}"), DataType::Int4, i).not_null())
            .collect();
        let def = catalog
            .create_table(public.oid, "t", defs, pk.unwrap_or_default(), false, false)
            .unwrap();
        let storage = Storage::new();
        let table = storage.create_table(def);
        (catalog, table)
    }

    fn row(values: &[i32]) -> Row {
        values.iter().map(|v| Value::Int4(*v)).collect()
    }

    #[test]
    fn insert_and_delete_move_the_index_with_the_row() {
        let (catalog, table) = table(1, Some(vec![0]));
        let index = btree_index(catalog.next_oid(), table.def().oid, "pkey", vec![0]).unique();
        let index = catalog
            .register_index(index.unique().primary_key())
            .unwrap();
        table.attach_index(index.clone());
        assert_eq!(index.columns, vec![0]);

        let rid = table.insert(row(&[1])).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(
            table.index_lookup(index.oid, &[Value::Int4(1)]),
            Some(vec![rid])
        );
        assert_eq!(table.def().rows(), 1);

        let removed = table.delete(rid).unwrap().unwrap();
        assert_eq!(removed, row(&[1]));
        assert_eq!(table.len(), 0);
        assert_eq!(
            table.index_lookup(index.oid, &[Value::Int4(1)]),
            Some(vec![])
        );
        assert_eq!(table.def().rows(), 0);
    }

    #[test]
    fn unique_indexes_reject_duplicates() {
        let (catalog, table) = table(2, None);
        let index = btree_index(catalog.next_oid(), table.def().oid, "uniq", vec![0, 1]).unique();
        table.attach_index(Arc::new(index));

        table.insert(row(&[1, 2])).unwrap();
        let error = table.insert(row(&[1, 2])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "duplicate key value violates unique constraint \"uniq\""
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn unique_indexes_allow_repeated_nulls() {
        // A nullable column, unlike the ones produced by the `table` helper.
        let catalog = Catalog::new();
        let public = catalog.resolve_schema("public").unwrap();
        let def = catalog
            .create_table(
                public.oid,
                "n",
                vec![ColumnDef::new("a", DataType::Int4, 0)],
                Vec::new(),
                false,
                false,
            )
            .unwrap();
        let storage = Storage::new();
        let nullable = storage.create_table(def);
        let index = btree_index(catalog.next_oid(), nullable.def().oid, "uniq", vec![0]).unique();
        nullable.attach_index(Arc::new(index));
        nullable.insert(vec![Value::Null]).unwrap();
        nullable.insert(vec![Value::Null]).unwrap();
        assert_eq!(nullable.len(), 2, "NULLs never collide in PostgreSQL");
    }

    #[test]
    fn not_null_is_enforced_by_the_engine() {
        let (_catalog, table) = table(1, None);
        let error = table.insert(vec![Value::Null]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "null value in column \"c0\" of relation \"t\" violates not-null constraint"
        );
    }

    #[test]
    fn update_rewrites_index_entries() {
        let (catalog, table) = table(1, None);
        let index = btree_index(catalog.next_oid(), table.def().oid, "idx", vec![0]);
        table.attach_index(Arc::new(index.clone()));
        let rid = table.insert(row(&[1])).unwrap();
        assert!(table.update(rid, row(&[7])).unwrap());
        assert_eq!(
            table.index_lookup(index.oid, &[Value::Int4(1)]),
            Some(vec![])
        );
        assert_eq!(
            table.index_lookup(index.oid, &[Value::Int4(7)]),
            Some(vec![rid])
        );
        assert_eq!(table.update(999, row(&[1])), Ok(false));
    }

    #[test]
    fn range_scans_follow_key_order() {
        let (catalog, table) = table(2, None);
        let index = btree_index(catalog.next_oid(), table.def().oid, "idx", vec![0, 1]);
        table.attach_index(Arc::new(index.clone()));
        for value in [5, 1, 9, 3] {
            table.insert(row(&[value, value * 10])).unwrap();
        }
        let all = table.index_range(index.oid, None, None).unwrap();
        assert_eq!(
            all.iter()
                .map(|rid| table.row(*rid).unwrap()[0].clone())
                .collect::<Vec<Value>>(),
            vec![
                Value::Int4(1),
                Value::Int4(3),
                Value::Int4(5),
                Value::Int4(9)
            ]
        );
        let inner = table
            .index_range(index.oid, Some(&Value::Int4(3)), Some(&Value::Int4(5)))
            .unwrap();
        assert_eq!(
            inner
                .iter()
                .map(|rid| table.row(*rid).unwrap()[0].clone())
                .collect::<Vec<Value>>(),
            vec![Value::Int4(3), Value::Int4(5)]
        );
    }

    #[test]
    fn hash_indexes_answer_equality_only() {
        let (catalog, table) = table(1, None);
        let mut def = btree_index(catalog.next_oid(), table.def().oid, "h", vec![0]);
        def.method = IndexMethod::Hash;
        table.attach_index(Arc::new(def.clone()));
        assert_eq!(table.index_method(def.oid), Some(IndexMethod::Hash));
        let rid = table.insert(row(&[42])).unwrap();
        assert_eq!(
            table.index_lookup(def.oid, &[Value::Int4(42)]),
            Some(vec![rid])
        );
        assert_eq!(table.index_range(def.oid, None, None), None);
    }

    #[test]
    fn truncate_empties_rows_and_indexes() {
        let (catalog, table) = table(1, None);
        let index = btree_index(catalog.next_oid(), table.def().oid, "idx", vec![0]);
        table.attach_index(Arc::new(index.clone()));
        table.insert(row(&[1])).unwrap();
        table.insert(row(&[2])).unwrap();
        table.truncate();
        assert!(table.is_empty());
        assert_eq!(table.index_key_count(index.oid), 0);
        assert_eq!(table.def().rows(), 0);
        table.insert(row(&[1])).unwrap();
        assert_eq!(
            table
                .index_lookup(index.oid, &[Value::Int4(1)])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn arity_is_validated() {
        let (_catalog, table) = table(2, None);
        let error = table.insert(row(&[1])).unwrap_err();
        assert!(error.to_string().contains("has 2 columns"), "{error}");
    }

    #[test]
    fn storage_registry_follows_oid_identity() {
        let (_catalog, table) = table(1, None);
        let oid = table.def().oid;
        let storage = Storage::new();
        assert!(storage.is_empty());
        storage.create_table(table.def().clone());
        assert_eq!(storage.len(), 1);
        assert!(storage.table(oid).is_some());
        assert!(storage.drop_table(oid).is_some());
        assert!(storage.table(oid).is_none());
    }
}
