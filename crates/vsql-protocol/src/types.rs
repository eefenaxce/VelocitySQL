//! Value encoding for the wire protocol.
//!
//! Both formats PostgreSQL defines are supported, because the client picks:
//!
//! * **text** (`format = 0`) is what `psql` asks for, and it matches
//!   `Value`'s `Display` — PostgreSQL's own output syntax (`bool_out` prints
//!   `t`/`f`, `numeric` keeps its declared scale).
//! * **binary** (`format = 1`) is what `lib/pq` asks for on the types it can
//!   decode natively (integers, floats, `bool`, the date/time family, ...). A
//!   client that requests it and receives text would read garbage, so the
//!   encoders below cover every scalar type the engine has; a type without one
//!   (an array) is reported back so the statement can be refused instead.
//!
//! `NULL` is not an encoding at all: the protocol transmits it as a length of
//! `-1`, which is why both encoders return `None` for it.

use chrono::{NaiveDate, NaiveDateTime, Timelike};
use rust_decimal::Decimal;
use vsql_types::{DataType, Value};

/// Encodes a value in PostgreSQL's text format.
///
/// Returns `None` for `NULL`, which the protocol transmits as a length of `-1`
/// rather than as an empty string — the two are different values.
pub fn encode_text(value: &Value) -> Option<Vec<u8>> {
    if value.is_null() {
        return None;
    }
    Some(value.to_string().into_bytes())
}

/// Encodes a value in the binary format of `data_type`.
///
/// `Ok(None)` is `NULL`. `Err` carries the type the engine has no binary
/// representation for, so the caller can refuse the statement rather than send
/// bytes the client would misread.
pub fn encode_binary(value: &Value, data_type: &DataType) -> Result<Option<Vec<u8>>, DataType> {
    if value.is_null() {
        return Ok(None);
    }
    let encoded = match value {
        // `bool` is a single byte, and the integers are big-endian.
        Value::Bool(flag) => vec![u8::from(*flag)],
        Value::Int2(number) => number.to_be_bytes().to_vec(),
        Value::Int4(number) => number.to_be_bytes().to_vec(),
        Value::Int8(number) => number.to_be_bytes().to_vec(),
        Value::Float4(number) => number.to_be_bytes().to_vec(),
        Value::Float8(number) => number.to_be_bytes().to_vec(),
        Value::Numeric(decimal) => encode_numeric(decimal),
        // The character types share one representation, and binary text *is*
        // the text.
        Value::Text(text) => text.as_bytes().to_vec(),
        Value::Bytes(bytes) => bytes.clone(),
        Value::Date(date) => (date.signed_duration_since(pg_epoch_date()).num_days() as i32)
            .to_be_bytes()
            .to_vec(),
        Value::Time(time) => {
            let micros = i64::from(time.num_seconds_from_midnight()) * 1_000_000
                + i64::from(time.nanosecond() / 1_000);
            micros.to_be_bytes().to_vec()
        }
        Value::Timestamp(datetime) => micros_since_pg_epoch(*datetime).to_be_bytes().to_vec(),
        Value::Timestamptz(datetime) => micros_since_pg_epoch(datetime.naive_utc())
            .to_be_bytes()
            .to_vec(),
        // `interval` keeps its triple: microseconds, then days, then months.
        Value::Interval(interval) => {
            let mut out = Vec::with_capacity(16);
            out.extend_from_slice(&interval.micros.to_be_bytes());
            out.extend_from_slice(&interval.days.to_be_bytes());
            out.extend_from_slice(&interval.months.to_be_bytes());
            out
        }
        Value::Uuid(uuid) => uuid.as_bytes().to_vec(),
        Value::Json(_) => {
            let text = value.to_string().into_bytes();
            if matches!(data_type, DataType::Jsonb) {
                // `jsonb_send` prefixes the version byte; `json` does not.
                let mut out = Vec::with_capacity(text.len() + 1);
                out.push(1);
                out.extend_from_slice(&text);
                out
            } else {
                text
            }
        }
        // Arrays have a binary form too (a header plus the elements), but the
        // engine has not needed it yet; saying so is better than guessing.
        Value::Array(_) | Value::Null => return Err(data_type.clone()),
    };
    Ok(Some(encoded))
}

/// PostgreSQL's epoch for `date` and `timestamp`: 2000-01-01.
fn pg_epoch_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2000, 1, 1).expect("the PostgreSQL epoch is a valid date")
}

/// Microseconds between `datetime` and PostgreSQL's epoch.
fn micros_since_pg_epoch(datetime: NaiveDateTime) -> i64 {
    let epoch = pg_epoch_date()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid");
    datetime
        .signed_duration_since(epoch)
        .num_microseconds()
        .unwrap_or_default()
}

/// Encodes a `numeric` in PostgreSQL's binary form.
///
/// The layout is `ndigits | weight | sign | dscale` followed by base-10000
/// digits, most significant first. `12345.678` is `3 | 1 | 0 | 3` followed by
/// `1, 2345, 6780`, and `0.00001` is `1 | -2 | 0 | 5 | 1000`.
fn encode_numeric(decimal: &Decimal) -> Vec<u8> {
    /// `NUMERIC_POS` / `NUMERIC_NEG`.
    const POSITIVE: u16 = 0x0000;
    const NEGATIVE: u16 = 0x4000;

    let sign = if decimal.is_sign_negative() {
        NEGATIVE
    } else {
        POSITIVE
    };
    let scale = decimal.scale() as usize;
    let mantissa = decimal.mantissa().unsigned_abs().to_string();
    // Left-pad so the integer part is never empty: `0.5` has the digits `05`.
    let digits = if mantissa.len() <= scale {
        format!("{mantissa:0>width$}", width = scale + 1)
    } else {
        mantissa
    };
    let split = digits.len() - scale;
    let integer = &digits[..split];
    let fraction = &digits[split..];

    let mut groups: Vec<u16> = Vec::new();
    // An integer part of `0` means every stored group is fractional, and the
    // first one sits one place below the point.
    let mut weight: i32 = -1;
    if integer != "0" {
        let padding = (4 - integer.len() % 4) % 4;
        let padded = format!("{}{integer}", "0".repeat(padding));
        weight = (padded.len() / 4) as i32 - 1;
        for chunk in padded.as_bytes().chunks(4) {
            groups.push(base_10000_group(chunk));
        }
    }
    if !fraction.is_empty() {
        let padding = (4 - fraction.len() % 4) % 4;
        let padded = format!("{fraction}{}", "0".repeat(padding));
        for chunk in padded.as_bytes().chunks(4) {
            groups.push(base_10000_group(chunk));
        }
    }
    // Leading zero groups carry no information.
    while groups.first() == Some(&0) {
        groups.remove(0);
        weight -= 1;
    }
    if groups.is_empty() {
        weight = 0;
    }

    let mut out = Vec::with_capacity(8 + groups.len() * 2);
    out.extend_from_slice(&(groups.len() as i16).to_be_bytes());
    out.extend_from_slice(&(weight as i16).to_be_bytes());
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&(scale as i16).to_be_bytes());
    for group in groups {
        out.extend_from_slice(&group.to_be_bytes());
    }
    out
}

/// Parses up to four ASCII digits — one base-10000 group.
fn base_10000_group(digits: &[u8]) -> u16 {
    digits
        .iter()
        .fold(0u16, |value, digit| value * 10 + u16::from(digit - b'0'))
}

/// `pg_type.typlen`: the fixed on-disk width, or `-1` for variable-length types.
pub fn type_len(data_type: &DataType) -> i16 {
    match data_type {
        DataType::Bool => 1,
        DataType::Int2 => 2,
        DataType::Int4 | DataType::Date => 4,
        DataType::Int8
        | DataType::Float8
        | DataType::Time
        | DataType::Timestamp
        | DataType::Timestamptz => 8,
        DataType::Interval | DataType::Uuid => 16,
        DataType::Float4 => 4,
        // `numeric` is variable because its display scale is part of the value.
        DataType::Numeric { .. } => -1,
        DataType::Text
        | DataType::Varchar(_)
        | DataType::Char(_)
        | DataType::Bytea
        | DataType::Json
        | DataType::Jsonb
        | DataType::Array(_) => -1,
    }
}

/// Decodes a text-format parameter into a value.
///
/// Used only for the parameter-free fast path of milestone M3: bound parameters
/// are still refused, but keeping the decoder here means parameter support is a
/// local change rather than a new subsystem.
pub fn decode_text(text: &str) -> Value {
    Value::Text(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nulls_encode_as_absent_not_empty() {
        assert_eq!(encode_text(&Value::Null), None);
        assert_eq!(
            encode_text(&Value::Text(String::new())),
            Some(Vec::new()),
            "an empty string is a value, not NULL"
        );
    }

    #[test]
    fn text_format_follows_postgres_output_syntax() {
        assert_eq!(
            encode_text(&Value::Bool(true)).unwrap(),
            b"t".to_vec(),
            "boolean text output is `t`, not `true`"
        );
        assert_eq!(encode_text(&Value::Int4(-7)).unwrap(), b"-7".to_vec());
        assert_eq!(
            encode_text(&Value::Text("ada".into())).unwrap(),
            b"ada".to_vec()
        );
    }

    #[test]
    fn typlen_matches_pg_type_catalog() {
        assert_eq!(type_len(&DataType::Int2), 2);
        assert_eq!(type_len(&DataType::Int8), 8);
        assert_eq!(type_len(&DataType::Uuid), 16);
        assert_eq!(type_len(&DataType::Text), -1);
        assert_eq!(
            type_len(&DataType::Numeric {
                precision: Some(10),
                scale: Some(2)
            }),
            -1
        );
    }

    /// The widths PostgreSQL's own `typsend` functions use.
    fn numeric() -> DataType {
        DataType::Numeric {
            precision: None,
            scale: None,
        }
    }

    #[test]
    fn binary_fixed_width_types_are_big_endian() {
        assert_eq!(
            encode_binary(&Value::Bool(true), &DataType::Bool).unwrap(),
            Some(vec![1])
        );
        assert_eq!(
            encode_binary(&Value::Int2(-2), &DataType::Int2).unwrap(),
            Some(vec![0xFF, 0xFE])
        );
        assert_eq!(
            encode_binary(&Value::Int4(7), &DataType::Int4).unwrap(),
            Some(vec![0, 0, 0, 7])
        );
        assert_eq!(
            encode_binary(&Value::Float8(1.5), &DataType::Float8).unwrap(),
            Some(1.5f64.to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_binary(&Value::Text("ada".into()), &DataType::Text).unwrap(),
            Some(b"ada".to_vec())
        );
        assert_eq!(encode_binary(&Value::Null, &DataType::Int4).unwrap(), None);
    }

    #[test]
    fn binary_dates_count_from_postgresqls_epoch() {
        let date = NaiveDate::from_ymd_opt(2000, 1, 2).expect("valid date");
        assert_eq!(
            encode_binary(&Value::Date(date), &DataType::Date).unwrap(),
            Some(1i32.to_be_bytes().to_vec())
        );
        let second = NaiveDate::from_ymd_opt(2000, 1, 1)
            .expect("valid date")
            .and_hms_opt(0, 0, 1)
            .expect("valid time");
        assert_eq!(
            encode_binary(&Value::Timestamp(second), &DataType::Timestamp).unwrap(),
            Some(1_000_000i64.to_be_bytes().to_vec())
        );
    }

    #[test]
    fn binary_numeric_uses_base_10000_digits() {
        // The layout `numeric_recv` documents: `ndigits | weight | sign |
        // dscale` then base-10000 groups.
        let big = Value::Numeric("12345.678".parse().expect("decimal"));
        assert_eq!(
            encode_binary(&big, &numeric()).unwrap(),
            Some(vec![0, 3, 0, 1, 0, 0, 0, 3, 0, 1, 0x09, 0x29, 0x1A, 0x7C])
        );
        // `0.00001` has no integer group, so its weight is -1 one place below
        // the point per skipped zero group.
        let small = Value::Numeric("0.00001".parse().expect("decimal"));
        assert_eq!(
            encode_binary(&small, &numeric()).unwrap(),
            Some(vec![0, 1, 0xFF, 0xFE, 0, 0, 0, 5, 0x03, 0xE8])
        );
        // A negative value flips the sign field (0x4000).
        let negative = Value::Numeric("-1.5".parse().expect("decimal"));
        assert_eq!(
            encode_binary(&negative, &numeric()).unwrap(),
            Some(vec![0, 2, 0, 0, 0x40, 0, 0, 1, 0, 1, 0x13, 0x88])
        );
        // Zero keeps its display scale but stores no digits.
        let zero = Value::Numeric("0.00".parse().expect("decimal"));
        assert_eq!(
            encode_binary(&zero, &numeric()).unwrap(),
            Some(vec![0, 0, 0, 0, 0, 0, 0, 2])
        );
    }
}
