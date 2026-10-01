//! PostgreSQL-compatible scalar data types.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A PostgreSQL logical data type as understood by VelocitySQL.
///
/// Types are value objects: they are cheap to clone and fully comparable, which
/// lets the catalog, planner and executor agree on type identity without any
/// interning machinery.
///
/// `Ord` is derived and used for deterministic ordering in `GROUP BY` keys and
/// type-resolution tie-breaking; it has no semantic meaning beyond stability.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DataType {
    /// `boolean`
    Bool,
    /// `smallint` / `int2`
    Int2,
    /// `integer` / `int4`
    Int4,
    /// `bigint` / `int8`
    Int8,
    /// `real` / `float4`
    Float4,
    /// `double precision` / `float8`
    Float8,
    /// `numeric(p, s)`
    Numeric {
        /// Declared precision, if any.
        precision: Option<u32>,
        /// Declared scale, if any.
        scale: Option<u32>,
    },
    /// `text`
    Text,
    /// `character varying(n)`
    Varchar(Option<u32>),
    /// `character(n)`
    Char(Option<u32>),
    /// `bytea`
    Bytea,
    /// `date`
    Date,
    /// `time without time zone`
    Time,
    /// `timestamp without time zone`
    Timestamp,
    /// `timestamp with time zone`
    Timestamptz,
    /// `interval`
    Interval,
    /// `uuid`
    Uuid,
    /// `json`
    Json,
    /// `jsonb`
    Jsonb,
    /// One-dimensional array of the wrapped element type.
    Array(Box<DataType>),
}

impl DataType {
    /// The canonical PostgreSQL type name, e.g. `int4` for `INTEGER`.
    pub fn name(&self) -> &'static str {
        match self {
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
        }
    }

    /// The `pg_type.oid` PostgreSQL assigns to this type.
    ///
    /// These constants matter for driver compatibility, not for storage.
    pub fn oid(&self) -> u32 {
        match self {
            DataType::Bool => 16,
            DataType::Int2 => 21,
            DataType::Int4 => 23,
            DataType::Int8 => 20,
            DataType::Float4 => 700,
            DataType::Float8 => 701,
            DataType::Numeric { .. } => 1700,
            DataType::Text => 25,
            DataType::Varchar(_) => 1043,
            DataType::Char(_) => 1042,
            DataType::Bytea => 17,
            DataType::Date => 1082,
            DataType::Time => 1083,
            DataType::Timestamp => 1114,
            DataType::Timestamptz => 1184,
            DataType::Interval => 1186,
            DataType::Uuid => 2950,
            DataType::Json => 114,
            DataType::Jsonb => 3802,
            DataType::Array(inner) => inner.array_oid(),
        }
    }

    /// The `pg_type.oid` of the array type whose element is `self`.
    pub fn array_oid(&self) -> u32 {
        match self {
            DataType::Bool => 1000,
            DataType::Bytea => 1001,
            DataType::Int2 => 1005,
            DataType::Int4 => 1007,
            DataType::Text => 1009,
            DataType::Char(_) => 1014,
            DataType::Varchar(_) => 1015,
            DataType::Int8 => 1016,
            DataType::Float4 => 1021,
            DataType::Float8 => 1022,
            DataType::Numeric { .. } => 1231,
            DataType::Timestamp => 1115,
            DataType::Date => 1182,
            DataType::Time => 1183,
            DataType::Timestamptz => 1185,
            DataType::Interval => 1187,
            DataType::Uuid => 2951,
            DataType::Json => 199,
            DataType::Jsonb => 3807,
            DataType::Array(_) => 0,
        }
    }

    /// `true` for `int2/int4/int8/float4/float8/numeric`.
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            DataType::Int2
                | DataType::Int4
                | DataType::Int8
                | DataType::Float4
                | DataType::Float8
                | DataType::Numeric { .. }
        )
    }

    /// `true` for `int2/int4/int8`.
    pub fn is_integer(&self) -> bool {
        matches!(self, DataType::Int2 | DataType::Int4 | DataType::Int8)
    }

    /// `true` for `text/varchar/bpchar`.
    pub fn is_textual(&self) -> bool {
        matches!(
            self,
            DataType::Text | DataType::Varchar(_) | DataType::Char(_)
        )
    }

    /// `true` for `date/time/timestamp/timestamptz/interval`.
    pub fn is_temporal(&self) -> bool {
        matches!(
            self,
            DataType::Date
                | DataType::Time
                | DataType::Timestamp
                | DataType::Timestamptz
                | DataType::Interval
        )
    }

    /// `true` when this is an array type.
    pub fn is_array(&self) -> bool {
        matches!(self, DataType::Array(_))
    }

    /// Returns the element type of an array type.
    pub fn element_type(&self) -> Option<&DataType> {
        match self {
            DataType::Array(inner) => Some(inner),
            _ => None,
        }
    }

    /// Resolves a type from a PostgreSQL type name or one of its common aliases.
    /// `true` for `text`, `varchar(n)` and `char(n)`.
    ///
    /// The three share one in-memory representation, so casts between them are
    /// representation-preserving and only differ in the declared length.
    pub fn is_character_type(&self) -> bool {
        matches!(
            self,
            DataType::Text | DataType::Varchar(_) | DataType::Char(_)
        )
    }

    /// Parses a PostgreSQL type name.
    pub fn from_name(name: &str) -> Option<DataType> {
        let lower = name.trim().to_ascii_lowercase();
        let base = lower.split('(').next().unwrap_or("").trim().to_string();
        let ty = match base.as_str() {
            "bool" | "boolean" => DataType::Bool,
            "int2" | "smallint" => DataType::Int2,
            "int" | "int4" | "integer" => DataType::Int4,
            "int8" | "bigint" => DataType::Int8,
            "float4" | "real" => DataType::Float4,
            "float8" | "double precision" | "double" | "float" => DataType::Float8,
            "numeric" | "decimal" | "dec" => DataType::Numeric {
                precision: None,
                scale: None,
            },
            "text" | "varchar" | "character varying" | "char varying" => DataType::Text,
            "bpchar" | "char" | "character" => DataType::Char(None),
            "bytea" => DataType::Bytea,
            "date" => DataType::Date,
            "time" | "time without time zone" => DataType::Time,
            "timestamp" | "timestamp without time zone" => DataType::Timestamp,
            "timestamptz" | "timestamp with time zone" => DataType::Timestamptz,
            "interval" => DataType::Interval,
            "uuid" => DataType::Uuid,
            "json" => DataType::Json,
            "jsonb" => DataType::Jsonb,
            "name" => DataType::Text,
            _ => return None,
        };
        Some(ty)
    }

    /// PostgreSQL's `format_type()`, used by `pg_attribute`, `\d` and error text.
    pub fn format_type(&self) -> String {
        match self {
            DataType::Bool => "boolean".to_string(),
            DataType::Int2 => "smallint".to_string(),
            DataType::Int4 => "integer".to_string(),
            DataType::Int8 => "bigint".to_string(),
            DataType::Float4 => "real".to_string(),
            DataType::Float8 => "double precision".to_string(),
            DataType::Numeric { precision, scale } => match (precision, scale) {
                (Some(p), Some(s)) => format!("numeric({p},{s})"),
                (Some(p), None) => format!("numeric({p})"),
                _ => "numeric".to_string(),
            },
            DataType::Text => "text".to_string(),
            DataType::Varchar(Some(n)) => format!("character varying({n})"),
            DataType::Varchar(None) => "character varying".to_string(),
            DataType::Char(Some(n)) => format!("character({n})"),
            DataType::Char(None) => "character".to_string(),
            DataType::Bytea => "bytea".to_string(),
            DataType::Date => "date".to_string(),
            DataType::Time => "time without time zone".to_string(),
            DataType::Timestamp => "timestamp without time zone".to_string(),
            DataType::Timestamptz => "timestamp with time zone".to_string(),
            DataType::Interval => "interval".to_string(),
            DataType::Uuid => "uuid".to_string(),
            DataType::Json => "json".to_string(),
            DataType::Jsonb => "jsonb".to_string(),
            DataType::Array(inner) => format!("{}[]", inner.format_type()),
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_type())
    }
}
