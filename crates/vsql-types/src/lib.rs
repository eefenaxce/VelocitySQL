//! VelocitySQL's type system.
//!
//! This crate is the foundation every other crate builds on. It answers three
//! questions and nothing else:
//!
//! 1. **What types exist?** — [`DataType`] mirrors PostgreSQL's scalar types,
//!    including the OIDs drivers expect.
//! 2. **How is a value represented?** — [`Value`] is a tagged union with a
//!    total order and a hash implementation, so the same type can be used as an
//!    index key, a hash-join key and a query result cell.
//! 3. **How do values compare and convert?** — [`Value::compare`] implements
//!    PostgreSQL operator resolution (including the `null`-propagating
//!    three-valued logic), while [`Value::cast_to`] implements the cast matrix.
//!
//! The crate has no I/O, no locking and no global state, which keeps it usable
//! from the catalog, storage, executor and protocol layers alike.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod data_type;
pub mod error;
pub mod interval;
pub mod value;

pub use data_type::DataType;
pub use error::TypeError;
pub use interval::Interval;
pub use value::{Row, Value};

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;
    use std::collections::HashSet;

    #[test]
    fn numeric_comparison_crosses_types() {
        let a = Value::Int4(1);
        let b = Value::Float8(1.5);
        assert_eq!(a.compare(&b, "<").unwrap(), Some(std::cmp::Ordering::Less));
        assert_eq!(
            Value::Int8(10)
                .compare(&Value::Numeric(10.into()), "=")
                .unwrap(),
            Some(std::cmp::Ordering::Equal)
        );
    }

    #[test]
    fn null_comparison_is_unknown() {
        assert_eq!(Value::Null.compare(&Value::Int4(1), "=").unwrap(), None);
        assert_eq!(Value::Int4(1).compare(&Value::Null, "=").unwrap(), None);
    }

    #[test]
    fn mismatched_types_error_like_postgres() {
        // PostgreSQL applies an implicit cast, so a non-numeric string compared
        // with an integer fails with "invalid input syntax", not a missing
        // operator.
        let err = Value::Int4(1)
            .compare(&Value::Text("x".into()), "=")
            .unwrap_err();
        assert!(err.to_string().contains("invalid input syntax"), "{err}");
    }

    #[test]
    fn string_adopts_numeric_counterpart_implicitly() {
        // `int_col = text_col` behaves like `int_col = 5`, PostgreSQL's
        // implicit cast.
        assert_eq!(
            Value::Int4(1)
                .compare(&Value::Text("1".into()), "=")
                .unwrap(),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Value::Text("2".into())
                .compare(&Value::Int4(2), "<")
                .unwrap(),
            Some(Ordering::Equal)
        );
        // A string that is not a number still has no comparison operator.
        assert!(Value::Int4(1)
            .compare(&Value::Text("x".into()), "=")
            .is_err());
    }

    #[test]
    fn string_adopts_boolean_counterpart_implicitly() {
        assert_eq!(
            Value::Bool(true)
                .compare(&Value::Text("t".into()), "=")
                .unwrap(),
            Some(Ordering::Equal)
        );
        assert_ne!(
            Value::Bool(false)
                .compare(&Value::Text("TRUE".into()), "=")
                .unwrap(),
            Some(Ordering::Equal)
        );
        assert!(Value::Bool(true)
            .compare(&Value::Text("maybe".into()), "=")
            .is_err());
    }

    #[test]
    fn text_casts_are_strict() {
        assert_eq!(
            Value::Text("42".into()).cast_to(&DataType::Int4).unwrap(),
            Value::Int4(42)
        );
        assert!(Value::Text("4x".into()).cast_to(&DataType::Int4).is_err());
        assert_eq!(
            Value::Text("true".into()).cast_to(&DataType::Bool).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn total_order_is_tag_then_value() {
        assert!(Value::Null < Value::Int4(i32::MIN));
        assert!(Value::Int4(999) < Value::Int8(0));
        // NaN is the largest float, matching PostgreSQL.
        assert!(Value::Float8(f64::NAN) > Value::Float8(f64::INFINITY));
    }

    #[test]
    fn hashing_matches_equality() {
        let mut set = HashSet::new();
        set.insert(Value::Float8(0.0));
        assert!(!set.insert(Value::Float8(-0.0)), "-0.0 must hash like 0.0");
        set.insert(Value::Float8(f64::NAN));
        assert!(!set.insert(Value::Float8(f64::NAN)), "NaN must equal NaN");
    }

    #[test]
    fn dates_and_timestamps_render_like_postgres() {
        let d = Value::Text("2024-02-29".into())
            .cast_to(&DataType::Date)
            .unwrap();
        assert_eq!(d.to_string(), "2024-02-29");
        let ts = Value::Text("2024-02-29 13:05:00".into())
            .cast_to(&DataType::Timestamp)
            .unwrap();
        assert_eq!(ts.to_string(), "2024-02-29 13:05:00");
    }

    #[test]
    fn intervals_round_trip() {
        let i = Interval::parse("1 year 2 mons 3 days 04:05:06").unwrap();
        assert_eq!(i.months, 14);
        assert_eq!(i.days, 3);
        assert_eq!(i.micros, (4 * 3600 + 5 * 60 + 6) * 1_000_000);
        assert_eq!(i.to_string(), "1 year 2 mons 3 days 04:05:06");
    }

    #[test]
    fn array_literals_split_into_elements() {
        let items = Value::parse_array_literal("{1,2,3}").unwrap();
        assert_eq!(
            items,
            vec![
                Value::Text("1".into()),
                Value::Text("2".into()),
                Value::Text("3".into())
            ]
        );
        // A quoted element keeps its commas; the empty literal is empty.
        let items = Value::parse_array_literal("{\"a,b\",c}").unwrap();
        assert_eq!(
            items,
            vec![Value::Text("a,b".into()), Value::Text("c".into())]
        );
        assert_eq!(
            Value::parse_array_literal("{}").unwrap(),
            Vec::<Value>::new()
        );
        // Not shaped like an array at all.
        assert!(Value::parse_array_literal("hello").is_none());
        assert!(Value::parse_array_literal("{unterminated").is_none());
    }

    #[test]
    fn only_an_array_target_reads_text_as_an_array() {
        // Every bind parameter arrives as text, and a driver renders an array
        // exactly like this.
        let tags = DataType::Array(Box::new(DataType::Text));
        assert_eq!(
            Value::Text("{a,b}".into()).cast_to(&tags).unwrap(),
            Value::Array(vec![Value::Text("a".into()), Value::Text("b".into())])
        );
        // The elements adopt the declared element type, which is what makes
        // `id = ANY($1)` with `{1,2}` match integers.
        let ids = DataType::Array(Box::new(DataType::Int4));
        assert_eq!(
            Value::Text("{1,2}".into()).cast_to(&ids).unwrap(),
            Value::Array(vec![Value::Int4(1), Value::Int4(2)])
        );
        assert_eq!(
            Value::Text("{}".into()).cast_to(&tags).unwrap(),
            Value::Array(Vec::new())
        );
        // Text that is not shaped like a literal is a single element.
        assert_eq!(
            Value::Text("plain".into()).cast_to(&tags).unwrap(),
            Value::Array(vec![Value::Text("plain".into())])
        );
        // The very same brace-shaped text bound for a `jsonb` column is a JSON
        // object: the array reading must not reach it.
        assert_eq!(
            Value::Text("{\"a\":1}".into())
                .cast_to(&DataType::Jsonb)
                .unwrap()
                .to_string(),
            "{\"a\":1}"
        );
    }
}
