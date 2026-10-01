//! VelocitySQL's catalog.
//!
//! The catalog is the authority on *what exists*: schemas, relations, columns,
//! indexes and sequences. It never stores rows — the storage engine does that —
//! which keeps the two concerns separable and lets `pg_catalog` be generated
//! from real definitions rather than from a parallel bookkeeping structure.
//!
//! Definitions are handed out as `Arc<TableDef>`: a statement resolves a name
//! once and then works against a stable snapshot even if a concurrent
//! `DROP TABLE` removes the entry.
//!
//! ```
//! use vsql_catalog::{Catalog, ColumnDef, btree_index};
//! use vsql_types::DataType;
//!
//! let catalog = Catalog::new();
//! let public = catalog.resolve_schema("public").unwrap();
//! let table = catalog
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
//!
//! let index = btree_index(catalog.next_oid(), table.oid, "users_pkey", vec![0]);
//! let index = catalog.register_index(index.unique().primary_key()).unwrap();
//! assert_eq!(index.columns, vec![0]);
//! assert_eq!(catalog.resolve_table(&["users".to_string()]).unwrap().name, "users");
//! ```
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod catalog;
pub mod defs;
pub mod error;
pub mod oid;
pub mod scram;

pub use catalog::{btree_index, Catalog};
pub use defs::{
    normalise_identifier, CheckDef, ColumnDef, DatabaseDef, ForeignKeyDef, IndexDef, IndexMethod,
    RoleDef, SchemaDef, SequenceDef, TableDef, MAX_IDENTIFIER_BYTES,
};
pub use error::CatalogError;
pub use oid::{oids, Oid};
pub use scram::ScramVerifier;

#[cfg(test)]
mod tests {
    use super::*;
    use vsql_types::DataType;

    fn setup() -> (Catalog, Oid) {
        let catalog = Catalog::new();
        let public = catalog.resolve_schema("public").unwrap();
        (catalog, public.oid)
    }

    #[test]
    fn default_schemas_match_postgres() {
        let catalog = Catalog::new();
        assert!(catalog.schema("pg_catalog").unwrap().is_system);
        assert!(
            catalog.schema("PUBLIC").is_some(),
            "names are case-insensitive"
        );
        assert!(catalog.schema("information_schema").unwrap().is_system);
    }

    #[test]
    fn oids_are_unique_and_never_reused() {
        let catalog = Catalog::new();
        let first = catalog.next_oid();
        let second = catalog.next_oid();
        assert!(second > first);
        assert!(first >= oids::FIRST_USER_OID);
    }

    #[test]
    fn duplicate_columns_are_rejected() {
        let (catalog, schema) = setup();
        let err = catalog
            .create_table(
                schema,
                "t",
                vec![
                    ColumnDef::new("a", DataType::Int4, 0),
                    ColumnDef::new("A", DataType::Int4, 1),
                ],
                Vec::new(),
                false,
                false,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "column \"A\" specified more than once");
    }

    #[test]
    fn create_table_honours_if_not_exists() {
        let (catalog, schema) = setup();
        let columns = vec![ColumnDef::new("a", DataType::Int4, 0)];
        let first = catalog
            .create_table(schema, "t", columns.clone(), Vec::new(), false, false)
            .unwrap();
        let again = catalog
            .create_table(schema, "t", columns.clone(), Vec::new(), true, false)
            .unwrap();
        assert_eq!(first.oid, again.oid);
        let err = catalog
            .create_table(schema, "t", columns, Vec::new(), false, false)
            .unwrap_err();
        assert_eq!(err.to_string(), "relation \"t\" already exists");
    }

    #[test]
    fn unqualified_names_resolve_through_the_search_path() {
        let (catalog, schema) = setup();
        catalog
            .create_table(
                schema,
                "t",
                vec![ColumnDef::new("a", DataType::Int4, 0)],
                Vec::new(),
                false,
                false,
            )
            .unwrap();
        let by_short = catalog.resolve_table(&["t".to_string()]).unwrap();
        let by_long = catalog
            .resolve_table(&["public".to_string(), "t".to_string()])
            .unwrap();
        assert_eq!(by_short.oid, by_long.oid);
        assert!(catalog
            .resolve_table(&["nope".to_string()])
            .unwrap_err()
            .to_string()
            .contains("does not exist"));
    }

    #[test]
    fn index_lookup_matches_leading_columns() {
        let (catalog, schema) = setup();
        let table = catalog
            .create_table(
                schema,
                "t",
                vec![
                    ColumnDef::new("a", DataType::Int4, 0),
                    ColumnDef::new("b", DataType::Int4, 1),
                ],
                vec![0],
                false,
                false,
            )
            .unwrap();
        let index = btree_index(catalog.next_oid(), table.oid, "t_a_b_idx", vec![0, 1]);
        catalog.register_index(index).unwrap();
        assert!(catalog.index_for_columns(table.oid, &[0]).is_some());
        assert!(catalog.index_for_columns(table.oid, &[0, 1]).is_some());
        assert!(catalog.index_for_columns(table.oid, &[1]).is_none());
        assert_eq!(catalog.table_indexes(table.oid).len(), 1);
    }

    #[test]
    fn dropping_a_table_removes_its_indexes() {
        let (catalog, schema) = setup();
        let table = catalog
            .create_table(
                schema,
                "t",
                vec![ColumnDef::new("a", DataType::Int4, 0)],
                Vec::new(),
                false,
                false,
            )
            .unwrap();
        let index = btree_index(catalog.next_oid(), table.oid, "t_idx", vec![0]);
        catalog.register_index(index).unwrap();
        let dropped = catalog.drop_table(schema, "t", false).unwrap().unwrap();
        assert_eq!(dropped.oid, table.oid);
        assert!(catalog.table_indexes(table.oid).is_empty());
        assert!(catalog.drop_table(schema, "t", true).unwrap().is_none());
    }

    #[test]
    fn sequences_advance_atomically() {
        let (catalog, schema) = setup();
        let sequence = catalog.create_sequence(schema, "s", 10, 5, false).unwrap();
        assert_eq!(sequence.nextval(), 10);
        assert_eq!(sequence.nextval(), 15);
        assert_eq!(catalog.nextval(schema, "s"), Some(20));
    }
}
