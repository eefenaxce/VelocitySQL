//! Object identifiers.

/// A catalog object identifier, mirroring PostgreSQL's `Oid` (32-bit unsigned).
pub type Oid = u32;

/// Well-known OIDs that PostgreSQL clients rely on.
pub mod oids {
    use super::Oid;

    /// `pg_catalog` namespace.
    pub const PG_CATALOG_NAMESPACE: Oid = 11;
    /// `public` namespace.
    pub const PUBLIC_NAMESPACE: Oid = 2200;
    /// `information_schema` namespace.
    pub const INFORMATION_SCHEMA_NAMESPACE: Oid = 13_000;
    /// First OID handed out to user objects.
    ///
    /// PostgreSQL's own catalog OIDs live below this value; keeping user objects
    /// above it avoids collisions once `pg_catalog` becomes a real relation set.
    pub const FIRST_USER_OID: Oid = 16_384;
}
