//! Runtime value representation.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use uuid::Uuid;

use crate::data_type::DataType;
use crate::error::TypeError;
use crate::interval::Interval;
use serde::{Deserialize, Serialize};

/// A row is simply a vector of values, positioned by column ordinal.
///
/// The executor deliberately avoids a `Schema` reference inside every row: the
/// operator that produced the row already owns the schema, so row materialisation
/// stays allocation-minimal.
pub type Row = Vec<Value>;

/// A single SQL value.
///
/// `Value` owns its payload. PostgreSQL uses varlena pointers, but in an
/// in-memory engine the extra indirection costs more than it saves; ownership
/// keeps the hot paths free of lifetime plumbing.
///
/// # Equality, ordering and hashing
///
/// [`Value`] implements `Eq`/`Ord`/`Hash` as a *total* order over a tagged
/// union: values of different types are ordered by their type tag and never
/// compare equal. That contract is what index keys and `GROUP BY` hashing need.
///
/// SQL-level semantics (where `1 = 1.0` and where mismatched types are an
/// error) live in [`Value::compare`], which implements PostgreSQL's operator
/// resolution rules instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// `boolean`
    Bool(bool),
    /// `smallint`
    Int2(i16),
    /// `integer`
    Int4(i32),
    /// `bigint`
    Int8(i64),
    /// `real`
    Float4(f32),
    /// `double precision`
    Float8(f64),
    /// `numeric`
    Numeric(#[serde(with = "decimal_as_text")] Decimal),
    /// `text`, `varchar` and `bpchar` share one physical representation.
    Text(String),
    /// `bytea`
    Bytes(Vec<u8>),
    /// `date`
    Date(NaiveDate),
    /// `time without time zone`
    Time(NaiveTime),
    /// `timestamp without time zone`
    Timestamp(NaiveDateTime),
    /// `timestamp with time zone`, normalised to UTC.
    Timestamptz(DateTime<Utc>),
    /// `interval`
    Interval(Interval),
    /// `uuid`
    Uuid(Uuid),
    /// `json` / `jsonb`
    Json(#[serde(with = "json_as_text")] JsonValue),
    /// array value, one-dimensional.
    Array(Vec<Value>),
}

// ------------------------------------------------------------------- serde
//
// Two payloads carry their own `Deserialize` which asks the *format* to
// describe the value (`deserialize_any`): `rust_decimal`'s and `serde_json`'s.
// That is a request a self-describing encoding can honour and a compact one
// cannot, so `bincode` — which the snapshot uses — fails while *reading* a file
// it wrote happily. Both therefore travel as the text PostgreSQL prints and
// accepts, which also keeps a `numeric`'s declared scale and a document's exact
// spelling.

/// Serde adapter: a `Decimal` travels as its text form.
mod decimal_as_text {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Decimal, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Serde adapter: a JSON document travels as its text form.
mod json_as_text {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::Value as JsonValue;

    pub fn serialize<S: Serializer>(value: &JsonValue, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<JsonValue, D::Error> {
        let text = String::deserialize(deserializer)?;
        serde_json::from_str(&text).map_err(serde::de::Error::custom)
    }
}

impl Value {
    /// Discriminant used as the primary key of the total order.
    fn tag(&self) -> u8 {
        match self {
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

    /// `true` when the value is SQL `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The SQL type of this value.
    pub fn type_of(&self) -> DataType {
        match self {
            Value::Null => DataType::Text,
            Value::Bool(_) => DataType::Bool,
            Value::Int2(_) => DataType::Int2,
            Value::Int4(_) => DataType::Int4,
            Value::Int8(_) => DataType::Int8,
            Value::Float4(_) => DataType::Float4,
            Value::Float8(_) => DataType::Float8,
            Value::Numeric(_) => DataType::Numeric {
                precision: None,
                scale: None,
            },
            Value::Text(_) => DataType::Text,
            Value::Bytes(_) => DataType::Bytea,
            Value::Date(_) => DataType::Date,
            Value::Time(_) => DataType::Time,
            Value::Timestamp(_) => DataType::Timestamp,
            Value::Timestamptz(_) => DataType::Timestamptz,
            Value::Interval(_) => DataType::Interval,
            Value::Uuid(_) => DataType::Uuid,
            Value::Json(_) => DataType::Jsonb,
            Value::Array(items) => {
                let elem = items
                    .iter()
                    .find(|v| !v.is_null())
                    .map(|v| v.type_of())
                    .unwrap_or(DataType::Text);
                DataType::Array(Box::new(elem))
            }
        }
    }

    /// Interprets the value as a boolean, following PostgreSQL's cast rules.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Interprets the value as a 64-bit integer.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int2(v) => Some(*v as i64),
            Value::Int4(v) => Some(*v as i64),
            Value::Int8(v) => Some(*v),
            Value::Float4(v) => Some(*v as i64),
            Value::Float8(v) => Some(*v as i64),
            Value::Numeric(v) => v.to_i64(),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    /// Interprets the value as a 64-bit float.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int2(v) => Some(*v as f64),
            Value::Int4(v) => Some(*v as f64),
            Value::Int8(v) => Some(*v as f64),
            Value::Float4(v) => Some(*v as f64),
            Value::Float8(v) => Some(*v),
            Value::Numeric(v) => v.to_f64(),
            _ => None,
        }
    }

    /// Borrows the value as a decimal, promoting integers.
    pub fn as_decimal(&self) -> Option<Decimal> {
        match self {
            Value::Int2(v) => Some(Decimal::from(*v)),
            Value::Int4(v) => Some(Decimal::from(*v)),
            Value::Int8(v) => Some(Decimal::from(*v)),
            Value::Numeric(v) => Some(*v),
            Value::Float4(v) => Decimal::from_f32_retain(*v),
            Value::Float8(v) => Decimal::from_f64_retain(*v),
            _ => None,
        }
    }

    /// Borrows the value as text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// `true` when the value is a numeric type.
    pub fn is_numeric(&self) -> bool {
        self.type_of().is_numeric()
    }

    // ---------------------------------------------------------------- ordering

    /// Compares two values with PostgreSQL's operator semantics.
    ///
    /// - `Ok(None)` means at least one side is `NULL` (SQL's unknown result).
    /// - `Err(..)` means the two types have no comparison operator, which
    ///   PostgreSQL reports as `operator does not exist`.
    pub fn compare(&self, other: &Value, op: &str) -> Result<Option<Ordering>, TypeError> {
        if self.is_null() || other.is_null() {
            return Ok(None);
        }
        if let (Some(a), Some(b)) = (self.as_decimal(), other.as_decimal()) {
            return Ok(Some(a.cmp(&b)));
        }
        let ord = match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            (Value::Date(a), Value::Date(b)) => a.cmp(b),
            (Value::Time(a), Value::Time(b)) => a.cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            (Value::Timestamptz(a), Value::Timestamptz(b)) => a.cmp(b),
            (Value::Date(a), Value::Timestamp(b)) => {
                a.and_hms_opt(0, 0, 0).expect("midnight is valid").cmp(b)
            }
            (Value::Timestamp(a), Value::Date(b)) => {
                a.cmp(&b.and_hms_opt(0, 0, 0).expect("midnight is valid"))
            }
            (Value::Date(a), Value::Timestamptz(b)) => {
                let a = a.and_hms_opt(0, 0, 0).expect("midnight is valid").and_utc();
                a.cmp(b)
            }
            (Value::Timestamptz(a), Value::Date(b)) => {
                let b = b.and_hms_opt(0, 0, 0).expect("midnight is valid").and_utc();
                a.cmp(&b)
            }
            (Value::Timestamp(a), Value::Timestamptz(b)) => a.and_utc().cmp(b),
            (Value::Timestamptz(a), Value::Timestamp(b)) => a.cmp(&b.and_utc()),
            (Value::Interval(a), Value::Interval(b)) => a.cmp(b),
            (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
            (Value::Json(a), Value::Json(b)) => json_to_string(a).cmp(&json_to_string(b)),
            (Value::Array(a), Value::Array(b)) => cmp_arrays(a, b),
            (Value::Null, _) | (_, Value::Null) => Ordering::Equal,
            _ => {
                // PostgreSQL applies an implicit cast to the operands of a
                // comparison, so a string against a number or boolean is read
                // as that type: `int_col = text_col` matches `int_col = 5`, and
                // `bool_col = 't'` matches `bool_col = true`.
                if let Some(result) = implicit_compare(self, other) {
                    return result.map(Some);
                }
                return Err(TypeError::UndefinedComparison {
                    left: self.type_of().format_type(),
                    op: op.to_string(),
                    right: other.type_of().format_type(),
                });
            }
        };
        Ok(Some(ord))
    }

    /// Compares two values for equality with three-valued logic.
    ///
    /// Returns `Ok(None)` when either side is `NULL`.
    pub fn sql_eq(&self, other: &Value) -> Result<Option<bool>, TypeError> {
        Ok(self.compare(other, "=")?.map(|o| o == Ordering::Equal))
    }

    /// Splits a PostgreSQL array literal into its elements.
    ///
    /// `'{1,2,3}'`, the way a client renders an array, is read one element at a
    /// time: commas separate elements at the top level, an element may be quoted
    /// with `"` or `'` (a quoted element may contain commas), and an empty body
    /// is the empty array. Returns `None` when `text` is not shaped like an
    /// array literal at all, so callers can fall back to their own handling.
    pub fn parse_array_literal(text: &str) -> Option<Vec<Value>> {
        let inner = text.trim();
        let inner = inner.strip_prefix('{')?.strip_suffix('}')?.trim();
        let mut items = Vec::new();
        let mut current = String::new();
        let mut quote: Option<char> = None;
        let mut has_element = false;
        for ch in inner.chars() {
            match quote {
                Some(q) => {
                    if ch == q {
                        quote = None;
                    } else {
                        current.push(ch);
                    }
                }
                None => match ch {
                    '"' | '\'' => {
                        quote = Some(ch);
                        has_element = true;
                    }
                    ',' => {
                        items.push(Value::Text(current.trim().to_string()));
                        current.clear();
                        has_element = false;
                    }
                    _ => {
                        current.push(ch);
                        has_element = true;
                    }
                },
            }
        }
        if quote.is_some() {
            // Unterminated quote: not a well-formed literal.
            return None;
        }
        if has_element || !current.is_empty() {
            items.push(Value::Text(current.trim().to_string()));
        }
        Some(items)
    }
}

/// Applies PostgreSQL's implicit casts to the operands of a comparison when one
/// side is a string and the other a number or boolean.
///
/// Returns `None` when neither operand is a string that can adopt the other's
/// type, leaving the caller to report `operator does not exist`. When the
/// string cannot be read as the target type (e.g. `int_col = 'active'`) the
/// error mirrors PostgreSQL's `invalid input syntax`, not a missing operator.
fn implicit_compare(a: &Value, b: &Value) -> Option<Result<Ordering, TypeError>> {
    let numeric = |text: &str, num: Decimal, ty: &str| -> Result<Ordering, TypeError> {
        match Decimal::from_str(text.trim()) {
            Ok(parsed) => Ok(num.cmp(&parsed)),
            Err(_) => Err(TypeError::InvalidInputSyntax {
                ty: ty.to_string(),
                input: text.to_string(),
            }),
        }
    };
    let boolean = |text: &str, value: bool, ty: &str| -> Result<Ordering, TypeError> {
        let parsed = match text.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "yes" | "1" | "on" => true,
            "f" | "false" | "no" | "0" | "off" => false,
            _ => {
                return Err(TypeError::InvalidInputSyntax {
                    ty: ty.to_string(),
                    input: text.to_string(),
                })
            }
        };
        Ok(value.cmp(&parsed))
    };
    if a.as_decimal().is_some() && matches!(b, Value::Text(_)) {
        if let (Some(num), Value::Text(text)) = (a.as_decimal(), b) {
            let ty = a.type_of().format_type();
            return Some(numeric(text, num, &ty));
        }
    }
    if matches!(a, Value::Text(_)) && b.as_decimal().is_some() {
        if let (Value::Text(text), Some(num)) = (a, b.as_decimal()) {
            let ty = b.type_of().format_type();
            return Some(numeric(text, num, &ty));
        }
    }
    if matches!(
        (a, b),
        (Value::Bool(_), Value::Text(_)) | (Value::Text(_), Value::Bool(_))
    ) {
        let text = match (a, b) {
            (_, Value::Text(t)) | (Value::Text(t), _) => t,
            _ => unreachable!(),
        };
        let value = match (a, b) {
            (Value::Bool(x), _) => *x,
            (_, Value::Bool(x)) => *x,
            _ => unreachable!(),
        };
        return Some(boolean(text, value, "boolean"));
    }
    None
}

fn cmp_arrays(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = x.cmp(y);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

fn json_to_string(v: &JsonValue) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    /// Total order used for index keys: type tag first, then payload.
    ///
    /// `NULL` sorts lowest here; SQL-level `NULLS FIRST/LAST` handling is the
    /// sort operator's job, not the comparator's.
    fn cmp(&self, other: &Self) -> Ordering {
        let (ta, tb) = (self.tag(), other.tag());
        if ta != tb {
            return ta.cmp(&tb);
        }
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int2(a), Value::Int2(b)) => a.cmp(b),
            (Value::Int4(a), Value::Int4(b)) => a.cmp(b),
            (Value::Int8(a), Value::Int8(b)) => a.cmp(b),
            (Value::Float4(a), Value::Float4(b)) => cmp_f64(*a as f64, *b as f64),
            (Value::Float8(a), Value::Float8(b)) => cmp_f64(*a, *b),
            (Value::Numeric(a), Value::Numeric(b)) => a.cmp(b),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            (Value::Date(a), Value::Date(b)) => a.cmp(b),
            (Value::Time(a), Value::Time(b)) => a.cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            (Value::Timestamptz(a), Value::Timestamptz(b)) => a.cmp(b),
            (Value::Interval(a), Value::Interval(b)) => a.cmp(b),
            (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
            (Value::Json(a), Value::Json(b)) => json_to_string(a).cmp(&json_to_string(b)),
            (Value::Array(a), Value::Array(b)) => cmp_arrays(a, b),
            _ => Ordering::Equal,
        }
    }
}

/// float comparison with PostgreSQL semantics: `NaN` is the largest value.
fn cmp_f64(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

fn canon_f64(v: f64) -> f64 {
    if v.is_nan() {
        f64::NAN
    } else if v == 0.0 {
        0.0
    } else {
        v
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.tag().hash(state);
        match self {
            Value::Null => {}
            Value::Bool(v) => v.hash(state),
            Value::Int2(v) => v.hash(state),
            Value::Int4(v) => v.hash(state),
            Value::Int8(v) => v.hash(state),
            Value::Float4(v) => canon_f64(*v as f64).to_bits().hash(state),
            Value::Float8(v) => canon_f64(*v).to_bits().hash(state),
            Value::Numeric(v) => v.hash(state),
            Value::Text(v) => v.hash(state),
            Value::Bytes(v) => v.hash(state),
            Value::Date(v) => v.hash(state),
            Value::Time(v) => v.hash(state),
            Value::Timestamp(v) => v.hash(state),
            Value::Timestamptz(v) => v.hash(state),
            Value::Interval(v) => v.hash(state),
            Value::Uuid(v) => v.hash(state),
            Value::Json(v) => json_to_string(v).hash(state),
            Value::Array(items) => items.hash(state),
        }
    }
}

// ---------------------------------------------------------------------- casts

impl Value {
    /// Converts this value to `ty`, following PostgreSQL's cast rules.
    pub fn cast_to(&self, ty: &DataType) -> Result<Value, TypeError> {
        if self.is_null() {
            return Ok(Value::Null);
        }
        if ty.is_array() {
            return self.cast_to_array(ty);
        }
        // Casting a value to its own type is a no-op. Without this shortcut the
        // generic machinery below rejects e.g. `timestamptz -> timestamptz`,
        // which the executor relies on when it coerces a `DEFAULT` expression to
        // the column's declared type.
        let source = self.type_of();
        if source == *ty {
            return Ok(self.clone());
        }
        // The character types share one representation; the declared length is
        // enforced when a row is written, not when a value is cast.
        if source.is_character_type() && ty.is_character_type() {
            return Ok(Value::Text(self.to_string()));
        }
        let coerced = match ty {
            DataType::Bool => self.cast_to_bool(),
            DataType::Int2 => self.cast_to_int(ty, i16::MIN as i64, i16::MAX as i64)?,
            DataType::Int4 => self.cast_to_int(ty, i32::MIN as i64, i32::MAX as i64)?,
            DataType::Int8 => self.cast_to_int(ty, i64::MIN, i64::MAX)?,
            DataType::Float4 => self.cast_to_float(ty)?,
            DataType::Float8 => self.cast_to_float(ty)?,
            DataType::Numeric { scale, .. } => self.cast_to_numeric(*scale)?,
            DataType::Text | DataType::Varchar(_) | DataType::Char(_) => {
                Some(Value::Text(self.to_string()))
            }
            _ => return self.cast_to_special(ty),
        };
        coerced.ok_or_else(|| TypeError::cannot_cast(&self.type_of(), ty))
    }

    fn cast_to_special(&self, ty: &DataType) -> Result<Value, TypeError> {
        match (self, ty) {
            (_, DataType::Bytea) => Ok(match self {
                Value::Bytes(b) => Value::Bytes(b.clone()),
                Value::Text(s) => Value::Bytes(parse_bytea(s)?),
                _ => {
                    return Err(TypeError::cannot_cast(&self.type_of(), ty));
                }
            }),
            (Value::Text(s), DataType::Date) => Ok(Value::Date(parse_date(s)?)),
            (Value::Text(s), DataType::Time) => Ok(Value::Time(parse_time(s)?)),
            (Value::Text(s), DataType::Timestamp) => Ok(Value::Timestamp(parse_timestamp(s)?)),
            (Value::Timestamp(t), DataType::Timestamptz) => Ok(Value::Timestamptz(t.and_utc())),
            (Value::Timestamptz(t), DataType::Timestamp) => Ok(Value::Timestamp(t.naive_utc())),
            (Value::Date(d), DataType::Timestamp) => Ok(Value::Timestamp(
                d.and_hms_opt(0, 0, 0).expect("midnight is valid"),
            )),
            (Value::Timestamp(t), DataType::Date) => Ok(Value::Date(t.date())),
            (Value::Timestamptz(t), DataType::Date) => Ok(Value::Date(t.naive_utc().date())),
            (Value::Text(s), DataType::Timestamptz) => {
                Ok(Value::Timestamptz(parse_timestamptz(s)?))
            }
            (Value::Text(s), DataType::Interval) => Ok(Value::Interval(Interval::parse(s)?)),
            (Value::Text(s), DataType::Uuid) => {
                Ok(Value::Uuid(Uuid::parse_str(s.trim()).map_err(|_| {
                    TypeError::InvalidInputSyntax {
                        ty: "uuid".into(),
                        input: s.clone(),
                    }
                })?))
            }
            (Value::Text(s), DataType::Json | DataType::Jsonb) => {
                let parsed = serde_json::from_str(s).map_err(|e| {
                    TypeError::other(format!("invalid input syntax for type json: {e}"))
                })?;
                Ok(Value::Json(parsed))
            }
            (Value::Json(_), DataType::Text | DataType::Varchar(_) | DataType::Char(_)) => {
                Ok(Value::Text(self.to_string()))
            }
            _ => Err(TypeError::cannot_cast(&self.type_of(), ty)),
        }
    }

    fn cast_to_array(&self, ty: &DataType) -> Result<Value, TypeError> {
        let Some(elem) = ty.element_type() else {
            return Err(TypeError::cannot_cast(&self.type_of(), ty));
        };
        // Elements are cast one by one, so a value arriving as text — which is
        // how every bind parameter arrives — is still read as the element type
        // its column declares.
        let cast_all = |items: &[Value]| -> Result<Value, TypeError> {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(item.cast_to(elem)?);
            }
            Ok(Value::Array(out))
        };
        match self {
            Value::Array(items) => cast_all(items),
            // A text value is an array literal only when the *target* says so:
            // `'{1,2}'` cast to `int[]` is two elements, while the very same
            // shape bound for a `jsonb` column is a JSON object and never gets
            // here. Anything not shaped like a literal is one element.
            Value::Text(text) => match Value::parse_array_literal(text) {
                Some(items) => cast_all(&items),
                None => Ok(Value::Array(vec![self.cast_to(elem)?])),
            },
            _ => Ok(Value::Array(vec![self.cast_to(elem)?])),
        }
    }

    fn cast_to_bool(&self) -> Option<Value> {
        match self {
            Value::Bool(b) => Some(Value::Bool(*b)),
            Value::Int2(v) => Some(Value::Bool(*v != 0)),
            Value::Int4(v) => Some(Value::Bool(*v != 0)),
            Value::Int8(v) => Some(Value::Bool(*v != 0)),
            Value::Numeric(v) => Some(Value::Bool(!v.is_zero())),
            Value::Text(s) => parse_bool(s).map(Value::Bool),
            _ => None,
        }
    }

    fn cast_to_int(&self, ty: &DataType, min: i64, max: i64) -> Result<Option<Value>, TypeError> {
        let raw = match self {
            Value::Int2(v) => *v as i64,
            Value::Int4(v) => *v as i64,
            Value::Int8(v) => *v,
            Value::Bool(b) => *b as i64,
            Value::Float4(v) => v.round() as i64,
            Value::Float8(v) => v.round() as i64,
            Value::Numeric(v) => match v.round().to_i64() {
                Some(v) => v,
                None => {
                    return Err(TypeError::OutOfRange {
                        ty: ty.format_type(),
                    })
                }
            },
            Value::Text(s) => {
                let trimmed = s.trim();
                match trimmed.parse::<i64>() {
                    Ok(v) => v,
                    Err(_) => match trimmed.parse::<f64>() {
                        Ok(f) if f.is_finite() => f.round() as i64,
                        _ => {
                            return Err(TypeError::InvalidInputSyntax {
                                ty: ty.format_type(),
                                input: s.clone(),
                            })
                        }
                    },
                }
            }
            _ => return Ok(None),
        };
        if raw < min || raw > max {
            return Err(TypeError::OutOfRange {
                ty: ty.format_type(),
            });
        }
        Ok(Some(match ty {
            DataType::Int2 => Value::Int2(raw as i16),
            DataType::Int4 => Value::Int4(raw as i32),
            _ => Value::Int8(raw),
        }))
    }

    fn cast_to_float(&self, ty: &DataType) -> Result<Option<Value>, TypeError> {
        let raw = match self {
            Value::Int2(v) => *v as f64,
            Value::Int4(v) => *v as f64,
            Value::Int8(v) => *v as f64,
            Value::Float4(v) => *v as f64,
            Value::Float8(v) => *v,
            Value::Numeric(v) => match v.to_f64() {
                Some(v) => v,
                None => {
                    return Err(TypeError::OutOfRange {
                        ty: ty.format_type(),
                    })
                }
            },
            Value::Text(s) => match s.trim().parse::<f64>() {
                Ok(v) => v,
                Err(_) => {
                    return Err(TypeError::InvalidInputSyntax {
                        ty: ty.format_type(),
                        input: s.clone(),
                    })
                }
            },
            _ => return Ok(None),
        };
        Ok(Some(match ty {
            DataType::Float4 => Value::Float4(raw as f32),
            _ => Value::Float8(raw),
        }))
    }

    fn cast_to_numeric(&self, scale: Option<u32>) -> Result<Option<Value>, TypeError> {
        let dec = match self {
            Value::Int2(_) | Value::Int4(_) | Value::Int8(_) | Value::Numeric(_) => {
                self.as_decimal()
            }
            Value::Float4(v) => Decimal::from_f32_retain(*v),
            Value::Float8(v) => Decimal::from_f64_retain(*v),
            Value::Text(s) => match Decimal::from_str(s.trim()) {
                Ok(v) => Some(v),
                Err(_) => {
                    return Err(TypeError::InvalidInputSyntax {
                        ty: "numeric".into(),
                        input: s.clone(),
                    })
                }
            },
            _ => None,
        };
        Ok(dec.map(|d| match scale {
            Some(s) => Value::Numeric(d.round_dp(s)),
            None => Value::Numeric(d),
        }))
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "y" | "yes" | "on" | "1" => Some(true),
        "f" | "false" | "n" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn invalid(ty: &str, input: &str) -> TypeError {
    TypeError::InvalidInputSyntax {
        ty: ty.to_string(),
        input: input.to_string(),
    }
}

/// Parses a `date` literal in the formats PostgreSQL accepts.
pub fn parse_date(s: &str) -> Result<NaiveDate, TypeError> {
    let s = s.trim();
    for fmt in ["%Y-%m-%d", "%Y/%m/%d", "%d-%m-%Y", "%Y%m%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Ok(d);
        }
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(dt.date());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt.date());
    }
    Err(invalid("date", s))
}

/// Parses a `time` literal.
pub fn parse_time(s: &str) -> Result<NaiveTime, TypeError> {
    let s = s.trim();
    for fmt in [
        "%H:%M:%S%.f",
        "%H:%M:%S",
        "%H:%M",
        "%H:%M:%S%.f%:z",
        "%H:%M:%S%:z",
    ] {
        if let Ok(t) = NaiveTime::parse_from_str(s, fmt) {
            return Ok(t);
        }
    }
    Err(invalid("time without time zone", s))
}

/// The `timestamp` layouts PostgreSQL accepts without a zone.
const TIMESTAMP_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%d %H:%M",
    "%Y-%m-%dT%H:%M",
];

/// The same layouts with a zone offset: `%:z` for `+08:00` (the spelling
/// `lib/pq` sends) and `%#z` for the lenient `+08`/`+0800` forms.
const TIMESTAMP_OFFSET_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S%.f%:z",
    "%Y-%m-%d %H:%M:%S%:z",
    "%Y-%m-%dT%H:%M:%S%.f%:z",
    "%Y-%m-%dT%H:%M:%S%:z",
    "%Y-%m-%d %H:%M:%S%.f%#z",
    "%Y-%m-%d %H:%M:%S%#z",
    "%Y-%m-%dT%H:%M:%S%.f%#z",
    "%Y-%m-%dT%H:%M:%S%#z",
];

/// Parses a timestamp that carries a zone offset, in any of its spellings.
fn parse_offset_datetime(s: &str) -> Option<DateTime<chrono::FixedOffset>> {
    for fmt in TIMESTAMP_OFFSET_FORMATS {
        if let Ok(datetime) = DateTime::parse_from_str(s, fmt) {
            return Some(datetime);
        }
    }
    DateTime::parse_from_rfc3339(s).ok()
}

/// Parses a `timestamp` literal.
///
/// A zone offset is accepted and then ignored, which is what PostgreSQL does
/// once a literal has been determined to be `timestamp without time zone`
/// (`'2004-10-19 10:23:54+02'::timestamp` is `2004-10-19 10:23:54`).
pub fn parse_timestamp(s: &str) -> Result<NaiveDateTime, TypeError> {
    let s = s.trim();
    for fmt in TIMESTAMP_FORMATS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt);
        }
    }
    if let Some(datetime) = parse_offset_datetime(s) {
        return Ok(datetime.naive_local());
    }
    if let Ok(d) = parse_date(s) {
        return Ok(d.and_hms_opt(0, 0, 0).expect("midnight is valid"));
    }
    Err(invalid("timestamp without time zone", s))
}

/// Parses a `timestamptz` literal and normalises it to UTC.
///
/// Unlike [`parse_timestamp`], the offset *is* applied: that is the difference
/// between the two types. A literal without one is read in the session's zone,
/// which the engine keeps at UTC.
pub fn parse_timestamptz(s: &str) -> Result<DateTime<Utc>, TypeError> {
    let s = s.trim();
    if let Some(datetime) = parse_offset_datetime(s) {
        return Ok(datetime.with_timezone(&Utc));
    }
    match parse_timestamp(s) {
        Ok(dt) => Ok(dt.and_utc()),
        Err(_) => Err(invalid("timestamp with time zone", s)),
    }
}

fn parse_bytea(s: &str) -> Result<Vec<u8>, TypeError> {
    let s = s.trim();
    let hex = s.strip_prefix("\\x").unwrap_or(s);
    if hex.len() % 2 != 0 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid("bytea", s));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (pair[1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push(hi << 4 | lo);
    }
    Ok(out)
}

// ------------------------------------------------------------------- rendering

impl fmt::Display for Value {
    /// PostgreSQL's text output format (`interval_out`, `bool_out`, ...).
    ///
    /// `NULL` renders as the empty string, exactly like `psql` shows it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Bool(v) => f.write_str(if *v { "t" } else { "f" }),
            Value::Int2(v) => write!(f, "{v}"),
            Value::Int4(v) => write!(f, "{v}"),
            Value::Int8(v) => write!(f, "{v}"),
            Value::Float4(v) => f.write_str(&format_f64(*v as f64)),
            Value::Float8(v) => f.write_str(&format_f64(*v)),
            // The scale is part of the type: `numeric(10,2)` prints `100.50`,
            // and so does the literal `100.50`. Normalising here would lose it.
            Value::Numeric(v) => f.write_str(&v.to_string()),
            Value::Text(v) => f.write_str(v),
            Value::Bytes(v) => {
                f.write_str("\\x")?;
                for byte in v {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            Value::Date(v) => write!(f, "{v}"),
            Value::Time(v) => f.write_str(&format_time(*v)),
            Value::Timestamp(v) => {
                write!(f, "{}", v.date())?;
                f.write_str(" ")?;
                f.write_str(&format_time(v.time()))
            }
            Value::Timestamptz(v) => {
                // Values are normalised to UTC on the way in, so the offset is
                // always the server time zone, which VelocitySQL fixes to UTC.
                let naive = v.naive_utc();
                write!(f, "{} {} +00", naive.date(), format_time(naive.time()))
            }
            Value::Interval(v) => write!(f, "{v}"),
            Value::Uuid(v) => write!(f, "{v}"),
            Value::Json(v) => f.write_str(&json_to_string(v)),
            Value::Array(items) => {
                f.write_str("{")?;
                for (idx, item) in items.iter().enumerate() {
                    if idx > 0 {
                        f.write_str(",")?;
                    }
                    f.write_str(&array_element(item))?;
                }
                f.write_str("}")
            }
        }
    }
}

/// One element of an array's text form, quoted exactly when PostgreSQL quotes
/// it: for an empty element, a value that would read back as the `NULL`
/// keyword, embedded separators or leading & trailing whitespace.
fn array_element(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Text(text) => {
            let needs_quotes = text.is_empty()
                || text.eq_ignore_ascii_case("null")
                || text.trim() != text
                || text
                    .chars()
                    .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\'));
            if needs_quotes {
                format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                text.clone()
            }
        }
        other => other.to_string(),
    }
}

fn format_time(t: NaiveTime) -> String {
    if t.nanosecond() == 0 {
        t.format("%H:%M:%S").to_string()
    } else {
        let base = t.format("%H:%M:%S").to_string();
        let micros = t.nanosecond() / 1000;
        format!("{base}.{micros:06}")
    }
}

fn format_f64(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mut s = format!("{v}");
    if s.contains('e') || s.contains('E') {
        // Rust prints exponents as `1e20`; PostgreSQL prints `1e+20`.
        s = s.replace('e', "e+").replace("e+-", "e-");
    }
    s
}

impl Value {
    /// Renders the value as a SQL literal, e.g. for `EXPLAIN` output.
    pub fn to_sql_literal(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Bool(v) => (*v).to_string(),
            Value::Date(v) => format!("DATE '{v}'"),
            Value::Time(v) => format!("TIME '{}'", format_time(*v)),
            Value::Timestamp(v) => format!("TIMESTAMP '{v}'"),
            Value::Timestamptz(v) => format!("TIMESTAMPTZ '{}'", v.naive_utc()),
            Value::Interval(v) => format!("INTERVAL '{v}'"),
            Value::Uuid(v) => format!("'{v}'::uuid"),
            Value::Bytes(_) => format!("'{}'::bytea", self),
            Value::Json(_) => format!("'{}'::jsonb", self),
            Value::Array(items) => {
                let inner: Vec<String> = items.iter().map(|v| v.to_sql_literal()).collect();
                format!("ARRAY[{}]", inner.join(", "))
            }
            other => other.to_string(),
        }
    }

    /// Builds a `timestamp`-typed value from a naive datetime.
    pub fn timestamp(dt: NaiveDateTime) -> Value {
        Value::Timestamp(dt)
    }
}
