//! Scalar functions.
//!
//! Only the functions milestone M1 needs are implemented, but the dispatch table
//! is the single place that decides which names exist, so adding one is a local
//! change and unknown names always produce PostgreSQL's
//! `function f(...) does not exist` error rather than a silent `NULL`.
//!
//! Functions are *strict* by default: a `NULL` argument yields `NULL` without
//! calling the implementation. The exceptions (`coalesce`, `concat`,
//! `greatest`, `least`, `nullif`) are listed explicitly.

use chrono::{Datelike, Timelike, Utc};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use vsql_types::{DataType, Interval, Value};

use crate::catalog_scan::{constraint_def_sql, index_def_sql, view_oid};
use crate::error::ExecError;
use crate::expr::{EvalEnv, ScalarExpr};
use crate::field::Field;
use crate::render;

/// Calls a scalar function.
///
/// `statement_time` is the timestamp the statement started at, so that `now()`
/// is stable across a single statement exactly like in PostgreSQL.
pub fn call_scalar(name: &str, args: &[Value], env: &EvalEnv) -> Result<Value, ExecError> {
    let now = || env.statement_time.unwrap_or_else(Utc::now);
    let strict = !matches!(
        name,
        "coalesce" | "concat" | "concat_ws" | "greatest" | "least" | "nullif"
    );
    if strict && args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }

    let value = match name {
        "upper" => Value::Text(text_arg(args, 0, name)?.to_uppercase()),
        "lower" => Value::Text(text_arg(args, 0, name)?.to_lowercase()),
        "length" | "char_length" | "character_length" => {
            Value::Int4(text_arg(args, 0, name)?.chars().count() as i32)
        }
        "octet_length" => Value::Int4(text_arg(args, 0, name)?.len() as i32),
        "replace" => {
            let source = text_arg(args, 0, name)?;
            let from = text_arg(args, 1, name)?;
            let to = text_arg(args, 2, name)?;
            Value::Text(source.replace(from, to))
        }
        "position" => {
            let needle = text_arg(args, 0, name)?;
            let haystack = text_arg(args, 1, name)?;
            match haystack.find(needle) {
                Some(offset) => Value::Int4(haystack[..offset].chars().count() as i32 + 1),
                None => Value::Int4(0),
            }
        }
        "substring" | "substr" => substring(args)?,
        "btrim" | "trim" => trim(args, TrimSide::Both)?,
        "ltrim" => trim(args, TrimSide::Left)?,
        "rtrim" => trim(args, TrimSide::Right)?,
        "concat" => Value::Text(
            args.iter()
                .map(|value| match value {
                    Value::Null => String::new(),
                    other => other.to_string(),
                })
                .collect::<String>(),
        ),
        "concat_ws" => {
            let separator = text_arg(args, 0, name)?;
            let parts: Vec<String> = args[1..]
                .iter()
                .filter(|value| !value.is_null())
                .map(|value| value.to_string())
                .collect();
            Value::Text(parts.join(separator))
        }
        "abs" => match args.first() {
            Some(Value::Int2(v)) => Value::Int2(v.abs()),
            Some(Value::Int4(v)) => Value::Int4(v.abs()),
            Some(Value::Int8(v)) => Value::Int8(v.abs()),
            Some(Value::Float4(v)) => Value::Float4(v.abs()),
            Some(Value::Float8(v)) => Value::Float8(v.abs()),
            Some(Value::Numeric(v)) => Value::Numeric(v.abs()),
            other => return Err(type_error(name, other)),
        },
        "sign" => match args.first() {
            Some(Value::Numeric(v)) => Value::Numeric(if v.is_sign_negative() {
                Decimal::NEGATIVE_ONE
            } else {
                Decimal::ONE
            }),
            other => {
                let Some(value) = other.and_then(Value::as_f64) else {
                    return Err(type_error(name, other));
                };
                Value::Float8(value.signum())
            }
        },
        "ceil" | "ceiling" => numeric_unary(name, args, |value| value.ceil())?,
        "floor" => numeric_unary(name, args, |value| value.floor())?,
        "trunc" => numeric_unary(name, args, |value| value.trunc())?,
        "round" => {
            let value = decimal_arg(args, 0, name)?;
            let scale = match args.get(1) {
                Some(scale) => scale.as_i64().unwrap_or(0).clamp(0, 28) as u32,
                None => 0,
            };
            Value::Numeric(value.round_dp(scale))
        }
        "sqrt" => float_unary(name, args, f64::sqrt)?,
        "power" | "pow" => {
            let base = float_arg(args, 0, name)?;
            let exponent = float_arg(args, 1, name)?;
            Value::Float8(base.powf(exponent))
        }
        "mod" => {
            let left = args.first().and_then(Value::as_i64);
            let right = args.get(1).and_then(Value::as_i64);
            match (left, right) {
                (Some(left), Some(right)) if right != 0 => Value::Int8(left % right),
                (Some(_), Some(_)) => return Err(ExecError::DivisionByZero),
                _ => return Err(type_error(name, args.first())),
            }
        }
        "coalesce" => args
            .iter()
            .find(|value| !value.is_null())
            .cloned()
            .unwrap_or(Value::Null),
        "nullif" => {
            let left = args.first().cloned().unwrap_or(Value::Null);
            let right = args.get(1).cloned().unwrap_or(Value::Null);
            match left.compare(&right, "=")? {
                Some(std::cmp::Ordering::Equal) => Value::Null,
                _ => left,
            }
        }
        "greatest" | "least" => {
            let mut best: Option<Value> = None;
            for candidate in args {
                if candidate.is_null() {
                    continue;
                }
                best = Some(match best {
                    None => candidate.clone(),
                    Some(current) => {
                        let ordering = candidate
                            .compare(&current, ">")?
                            .unwrap_or(std::cmp::Ordering::Equal);
                        let take_candidate = if name == "greatest" {
                            ordering == std::cmp::Ordering::Greater
                        } else {
                            ordering == std::cmp::Ordering::Less
                        };
                        if take_candidate {
                            candidate.clone()
                        } else {
                            current
                        }
                    }
                });
            }
            best.unwrap_or(Value::Null)
        }
        "now" | "current_timestamp" | "transaction_timestamp" | "statement_timestamp" => {
            Value::Timestamptz(now())
        }
        "clock_timestamp" => Value::Timestamptz(Utc::now()),
        "current_date" => Value::Date(now().naive_utc().date()),
        "current_time" | "localtime" => Value::Time(now().naive_utc().time()),
        "timeofday" => Value::Text(now().to_rfc3339()),
        "extract" | "date_part" => extract(args)?,
        "version" => Value::Text(format!(
            "PostgreSQL 15.0 (VelocitySQL {})",
            env!("CARGO_PKG_VERSION")
        )),
        "current_schema" => Value::Text(
            env.catalog
                .as_ref()
                .map(|snapshot| snapshot.current_schema())
                .unwrap_or_else(|| "public".to_string()),
        ),
        // `n.nspname = ANY (current_schemas(false))` is how every client filters
        // the schemas it shows; `pg_catalog` counts as implicit.
        "current_schemas" => {
            let include_implicit = args.first().and_then(Value::as_bool).unwrap_or(false);
            let schemas = env
                .catalog
                .as_ref()
                .map(|snapshot| snapshot.current_schemas(include_implicit))
                .unwrap_or_else(|| vec!["public".to_string()]);
            Value::Array(schemas.into_iter().map(Value::Text).collect())
        }
        // Reads the same session parameters `pg_settings` materialises; a
        // parameter the session never set reports `NULL` (the engine accepts
        // more GUCs than it models, so erroring would break browsers that probe
        // optional ones).
        "current_setting" => {
            let name = text_arg(args, 0, name)?;
            Value::Text(
                env.catalog
                    .as_ref()
                    .and_then(|snapshot| {
                        snapshot
                            .settings
                            .iter()
                            .find(|(setting, _)| setting == name)
                            .map(|(_, value)| value.clone())
                    })
                    .unwrap_or_default(),
            )
        }
        "current_user" | "session_user" | "user" => Value::Text(
            env.user
                .clone()
                .unwrap_or_else(|| "velocitysql".to_string()),
        ),
        // Reported from the session's actual connection rather than a constant,
        // so it stays truthful once several databases exist.
        "current_database" => Value::Text(
            env.database
                .clone()
                .unwrap_or_else(|| "velocitysql".to_string()),
        ),
        "pg_backend_pid" => Value::Int4(std::process::id() as i32),
        // Catalogue helpers the tools call while browsing; they read the same
        // snapshot the virtual `pg_catalog` relations do.
        "pg_get_userbyid" => {
            let oid = match args.first() {
                Some(Value::Int4(oid)) => *oid as u32,
                Some(Value::Int8(oid)) => *oid as u32,
                Some(Value::Text(text)) => text.parse().unwrap_or(0),
                _ => 0,
            };
            Value::Text(
                env.catalog
                    .as_ref()
                    .and_then(|snapshot| snapshot.role_name(oid).map(str::to_string))
                    .unwrap_or_else(|| "?".to_string()),
            )
        }
        "pg_encoding_to_char" => Value::Text(match args.first() {
            Some(Value::Int4(0)) => "SQL_ASCII".to_string(),
            _ => "UTF8".to_string(),
        }),
        // `pg_get_expr(adbin, adrelid[, pretty])`: PostgreSQL renders the
        // `pg_node_tree` stored in `pg_attrdef.adbin`; the engine keeps column
        // defaults already rendered to SQL text, so the display form is the
        // stored string itself.
        "pg_get_expr" => Value::Text(match args.first() {
            Some(Value::Text(text)) => text.clone(),
            other => return Err(type_error(name, other)),
        }),
        // `format_type(typoid[, typmod])`: browsers call it for every column of
        // every table, feeding it `pg_attribute.atttypid` + `atttypmod`.
        "format_type" => {
            let oid = args.first().and_then(Value::as_i64).unwrap_or(0) as u32;
            let base = match type_from_oid(oid) {
                Some(base) => base,
                None => match array_element_oid(oid) {
                    Some(element) => {
                        return Ok(Value::Text(format!("{}[]", element.format_type())))
                    }
                    None => return Err(type_error(name, args.first())),
                },
            };
            // Decode `atttypmod` the way PostgreSQL encodes it: length types
            // store `n + 4`, `numeric` stores `((p << 16) | s) + 4`.
            let ty = match args.get(1).and_then(Value::as_i64) {
                Some(modifier) if modifier >= 4 => {
                    let decoded = (modifier - 4) as i32;
                    match base {
                        DataType::Varchar(_) => DataType::Varchar(Some(decoded as u32)),
                        DataType::Char(_) => DataType::Char(Some(decoded as u32)),
                        DataType::Numeric { .. } => DataType::Numeric {
                            precision: Some(((decoded >> 16) & 0xffff) as u32),
                            scale: Some((decoded & 0xffff) as u32),
                        },
                        other => other,
                    }
                }
                _ => base,
            };
            Value::Text(ty.format_type())
        }
        // `pg_get_viewdef(view[, pretty])`: the parsed query is kept verbatim,
        // so the definition renders back to the `SELECT` the user wrote.
        "pg_get_viewdef" => {
            let query = match args.first() {
                Some(Value::Text(name)) => {
                    let parts: Vec<String> = name.split('.').map(str::to_string).collect();
                    env.catalog
                        .as_ref()
                        .and_then(|snapshot| snapshot.database.view(&parts))
                }
                Some(Value::Int4(_) | Value::Int8(_)) => {
                    let oid = args.first().and_then(Value::as_i64).unwrap_or(0) as u32;
                    env.catalog.as_ref().and_then(|snapshot| {
                        snapshot
                            .database
                            .catalog
                            .schemas()
                            .iter()
                            .find_map(|schema| {
                                snapshot
                                    .database
                                    .views_in(schema.oid)
                                    .into_iter()
                                    .find(|view| view_oid(schema.oid, view) == oid)
                                    .and_then(|view| {
                                        snapshot.database.view(&[schema.name.clone(), view])
                                    })
                            })
                    })
                }
                _ => None,
            };
            Value::Text(query.map(|query| render::query(&query)).unwrap_or_default())
        }
        // `pg_get_indexdef(index_oid)`: the DDL browsers show on their index tab.
        "pg_get_indexdef" => {
            let oid = args.first().and_then(Value::as_i64).unwrap_or(0) as u32;
            let definition = env.catalog.as_ref().and_then(|snapshot| {
                snapshot.database.catalog.tables().iter().find_map(|table| {
                    snapshot
                        .database
                        .catalog
                        .table_indexes(table.oid)
                        .into_iter()
                        .find(|index| index.oid == oid)
                        .map(|index| index_def_sql(table, &index))
                })
            });
            Value::Text(definition.unwrap_or_default())
        }
        // `pg_get_constraintdef(oid[, pretty])`: the definition text browsers
        // show on a table's constraints tab, resolved by the oid the
        // `pg_constraint` scan reports. `pretty` is accepted and ignored: the
        // engine renders the same form either way.
        "pg_get_constraintdef" => {
            let oid = args.first().and_then(Value::as_i64).unwrap_or(0) as u32;
            let definition = env
                .catalog
                .as_ref()
                .and_then(|snapshot| constraint_def_sql(&snapshot.database.catalog, oid));
            Value::Text(definition.unwrap_or_default())
        }
        // Advances the named sequence; the atomically incremented value every
        // serial column default hands out. The argument may be schema
        // qualified, and an unqualified name is looked up across schemas
        // without advancing anything else.
        "nextval" => {
            let text = text_arg(args, 0, name)?;
            let Some(snapshot) = env.catalog.as_ref() else {
                return Err(ExecError::other(format!(
                    "relation \"{text}\" does not exist"
                )));
            };
            let catalog = &snapshot.database.catalog;
            let (schema_oid, sequence_name) = match text.rsplit_once('.') {
                Some((schema, sequence)) => {
                    (catalog.resolve_schema(schema)?.oid, sequence.to_string())
                }
                None => {
                    let sequence_name = text.to_string();
                    let schema_oid = catalog
                        .sequences()
                        .iter()
                        .find(|sequence| sequence.name == sequence_name)
                        .map(|sequence| sequence.schema_oid)
                        .ok_or_else(|| {
                            ExecError::other(format!("relation \"{text}\" does not exist"))
                        })?;
                    (schema_oid, sequence_name)
                }
            };
            match catalog.nextval(schema_oid, &sequence_name) {
                Some(value) => Value::Int8(value),
                None => {
                    return Err(ExecError::other(format!(
                        "relation \"{text}\" does not exist"
                    )));
                }
            }
        }
        // The sequence behind a serial column, as its schema-qualified name.
        "pg_get_serial_sequence" => {
            let table = text_arg(args, 0, name)?;
            let column = text_arg(args, 1, name)?;
            let sequence = format!("{}_{}_seq", table, column);
            let Some(snapshot) = env.catalog.as_ref() else {
                return Ok(Value::Null);
            };
            let catalog = &snapshot.database.catalog;
            let Some(def) = catalog
                .sequences()
                .into_iter()
                .find(|def| def.name == sequence)
            else {
                return Ok(Value::Null);
            };
            let schema = catalog
                .schemas()
                .into_iter()
                .find(|schema| schema.oid == def.schema_oid)
                .map(|schema| schema.name)
                .unwrap_or_default();
            Value::Text(format!("{schema}.{}", def.name))
        }
        // The engine runs standalone: recovery is never in progress.
        "pg_is_in_recovery" => Value::Bool(false),
        // Without per-relation visibility rules everything is visible, which is
        // the honest answer of an engine with no schemas a user cannot read.
        "pg_table_is_visible" => Value::Bool(true),
        // Description lookups: the engine has no `COMMENT ON` yet, so the
        // truthful answer for every object is "no description". Returning an
        // error here would break `\l+`-style queries that merely decorate their
        // output with an optional comment column.
        "shobj_description" | "obj_description" | "col_description" => Value::Null,
        "pg_typeof" => Value::Text(
            args.first()
                .map(|value| value.type_of().format_type())
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        other => {
            return Err(ExecError::UndefinedFunction(format!(
                "{other}({})",
                args.iter()
                    .map(|value| value.type_of().format_type())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    };
    Ok(value)
}

/// Best-effort static return type for a scalar function.
pub fn scalar_return_type(name: &str, args: &[ScalarExpr], schema: &[Field]) -> Option<DataType> {
    let arg_type = |index: usize| args.get(index).and_then(|arg| arg.infer_type(schema));
    let result = match name {
        "upper"
        | "lower"
        | "replace"
        | "substring"
        | "substr"
        | "btrim"
        | "trim"
        | "ltrim"
        | "rtrim"
        | "concat"
        | "concat_ws"
        | "timeofday"
        | "version"
        | "current_schema"
        | "current_user"
        | "session_user"
        | "user"
        | "current_database"
        | "pg_typeof"
        | "pg_get_userbyid"
        | "pg_encoding_to_char"
        | "pg_get_expr"
        | "format_type"
        | "pg_get_viewdef"
        | "pg_get_indexdef"
        | "pg_get_serial_sequence"
        | "pg_get_constraintdef" => Some(DataType::Text),
        "pg_table_is_visible" | "pg_is_in_recovery" => Some(DataType::Bool),
        "nextval" => Some(DataType::Int8),
        "current_schemas" => Some(DataType::Array(Box::new(DataType::Text))),
        "shobj_description" | "obj_description" | "col_description" => Some(DataType::Text),
        "length" | "char_length" | "character_length" | "octet_length" | "position"
        | "pg_backend_pid" => Some(DataType::Int4),
        "abs" | "greatest" | "least" => arg_type(0),
        "sign" => arg_type(0),
        "ceil" | "ceiling" | "floor" | "trunc" | "round" => arg_type(0),
        "sqrt" | "power" | "pow" => Some(DataType::Float8),
        "mod" => Some(DataType::Int8),
        "coalesce" => args.iter().find_map(|arg| arg.infer_type(schema)),
        "nullif" => arg_type(0),
        "now"
        | "current_timestamp"
        | "transaction_timestamp"
        | "statement_timestamp"
        | "clock_timestamp" => Some(DataType::Timestamptz),
        "current_date" => Some(DataType::Date),
        "current_time" | "localtime" => Some(DataType::Time),
        "extract" | "date_part" => Some(DataType::Numeric {
            precision: None,
            scale: None,
        }),
        _ => None,
    };
    result
}

/// Inverts `DataType::oid`, so `format_type` can rebuild the display name from
/// a `pg_type.oid`. Parameterised types come back without their parameters;
/// `atttypmod` restores them.
fn type_from_oid(oid: u32) -> Option<DataType> {
    Some(match oid {
        16 => DataType::Bool,
        17 => DataType::Bytea,
        20 => DataType::Int8,
        21 => DataType::Int2,
        23 => DataType::Int4,
        25 => DataType::Text,
        114 => DataType::Json,
        700 => DataType::Float4,
        701 => DataType::Float8,
        1042 => DataType::Char(None),
        1043 => DataType::Varchar(None),
        1082 => DataType::Date,
        1083 => DataType::Time,
        1114 => DataType::Timestamp,
        1184 => DataType::Timestamptz,
        1186 => DataType::Interval,
        1700 => DataType::Numeric {
            precision: None,
            scale: None,
        },
        2950 => DataType::Uuid,
        3802 => DataType::Jsonb,
        _ => return None,
    })
}

/// The element type of an array `pg_type.oid`, for `format_type(int4array)`.
fn array_element_oid(oid: u32) -> Option<DataType> {
    Some(match oid {
        199 => DataType::Json,
        1000 => DataType::Bool,
        1001 => DataType::Bytea,
        1005 => DataType::Int2,
        1007 => DataType::Int4,
        1009 => DataType::Text,
        1014 => DataType::Char(None),
        1015 => DataType::Varchar(None),
        1016 => DataType::Int8,
        1021 => DataType::Float4,
        1022 => DataType::Float8,
        1231 => DataType::Numeric {
            precision: None,
            scale: None,
        },
        1115 => DataType::Timestamp,
        1182 => DataType::Date,
        1183 => DataType::Time,
        1185 => DataType::Timestamptz,
        1187 => DataType::Interval,
        2951 => DataType::Uuid,
        3807 => DataType::Jsonb,
        _ => return None,
    })
}

fn type_error(name: &str, value: Option<&Value>) -> ExecError {
    let ty = value
        .map(|value| value.type_of().format_type())
        .unwrap_or_else(|| "unknown".to_string());
    ExecError::UndefinedFunction(format!("{name}({ty})"))
}

fn text_arg<'a>(args: &'a [Value], index: usize, name: &str) -> Result<&'a str, ExecError> {
    match args.get(index).and_then(Value::as_str) {
        Some(text) => Ok(text),
        None => Err(type_error(name, args.get(index))),
    }
}

fn float_arg(args: &[Value], index: usize, name: &str) -> Result<f64, ExecError> {
    args.get(index)
        .and_then(Value::as_f64)
        .ok_or_else(|| type_error(name, args.get(index)))
}

fn decimal_arg(args: &[Value], index: usize, name: &str) -> Result<Decimal, ExecError> {
    args.get(index)
        .and_then(Value::as_decimal)
        .ok_or_else(|| type_error(name, args.get(index)))
}

fn numeric_unary(
    name: &str,
    args: &[Value],
    f: impl Fn(Decimal) -> Decimal,
) -> Result<Value, ExecError> {
    let value = decimal_arg(args, 0, name)?;
    Ok(Value::Numeric(f(value)))
}

fn float_unary(name: &str, args: &[Value], f: impl Fn(f64) -> f64) -> Result<Value, ExecError> {
    let value = float_arg(args, 0, name)?;
    Ok(Value::Float8(f(value)))
}

fn substring(args: &[Value]) -> Result<Value, ExecError> {
    let source = text_arg(args, 0, "substring")?;
    let start = args
        .get(1)
        .and_then(Value::as_i64)
        .ok_or_else(|| type_error("substring", args.get(1)))?;
    let chars: Vec<char> = source.chars().collect();
    // SQL's `substring` is 1-based and clamps both ends instead of erroring.
    let from = (start.max(1) as usize - 1).min(chars.len());
    let to = match args.get(2).and_then(Value::as_i64) {
        Some(length) if length >= 0 => (from + length as usize).min(chars.len()),
        Some(_) => return Err(ExecError::other("negative substring length not allowed")),
        None => chars.len(),
    };
    Ok(Value::Text(chars[from..to].iter().collect()))
}

enum TrimSide {
    Both,
    Left,
    Right,
}

fn trim(args: &[Value], side: TrimSide) -> Result<Value, ExecError> {
    let name = match side {
        TrimSide::Both => "btrim",
        TrimSide::Left => "ltrim",
        TrimSide::Right => "rtrim",
    };
    let source = text_arg(args, 0, name)?;
    let characters = match args.get(1) {
        Some(_) => text_arg(args, 1, name)?,
        None => " ",
    };
    let characters: Vec<char> = characters.chars().collect();
    let is_trim = |c: char| characters.contains(&c);
    let trimmed = match side {
        TrimSide::Both => source.trim_matches(is_trim),
        TrimSide::Left => source.trim_start_matches(is_trim),
        TrimSide::Right => source.trim_end_matches(is_trim),
    };
    Ok(Value::Text(trimmed.to_string()))
}

/// Implements `EXTRACT(field FROM value)` / `date_part(field, value)`.
fn extract(args: &[Value]) -> Result<Value, ExecError> {
    let field = text_arg(args, 0, "extract")?.to_ascii_lowercase();
    let Some(value) = args.get(1) else {
        return Err(type_error("extract", None));
    };
    let number = |value: i64| Value::Numeric(Decimal::from(value));
    match value {
        Value::Date(date) => {
            let naive = date.and_hms_opt(0, 0, 0).expect("midnight is valid");
            extract_datetime(&field, naive)
        }
        Value::Timestamp(naive) => extract_datetime(&field, *naive),
        Value::Timestamptz(instant) => extract_datetime(&field, instant.naive_utc()),
        Value::Time(time) => match field.as_str() {
            "hour" => Ok(number(time.hour() as i64)),
            "minute" => Ok(number(time.minute() as i64)),
            "second" => Ok(Value::Numeric(
                Decimal::from(time.second())
                    + Decimal::from(time.nanosecond() / 1_000) / Decimal::from(1_000_000),
            )),
            other => Err(ExecError::other(format!(
                "unit \"{other}\" not supported for type time"
            ))),
        },
        Value::Interval(interval) => extract_interval(&field, interval),
        other => Err(ExecError::other(format!(
            "unit \"{field}\" not supported for type {}",
            other.type_of().format_type()
        ))),
    }
}

fn extract_datetime(field: &str, value: chrono::NaiveDateTime) -> Result<Value, ExecError> {
    let number = |value: i64| Value::Numeric(Decimal::from(value));
    let result = match field {
        "year" => number(value.year() as i64),
        "month" => number(value.month() as i64),
        "day" => number(value.day() as i64),
        "hour" => number(value.hour() as i64),
        "minute" => number(value.minute() as i64),
        "second" => Value::Numeric(
            Decimal::from(value.second())
                + Decimal::from(value.nanosecond() / 1_000) / Decimal::from(1_000_000),
        ),
        "dow" => number(value.weekday().num_days_from_sunday() as i64),
        "isodow" => number(value.weekday().number_from_monday() as i64),
        "doy" => number(value.ordinal() as i64),
        "quarter" => number(((value.month() - 1) / 3 + 1) as i64),
        "epoch" => number(value.and_utc().timestamp()),
        other => {
            return Err(ExecError::other(format!(
                "unit \"{other}\" not supported for type timestamp"
            )))
        }
    };
    Ok(result)
}

fn extract_interval(field: &str, interval: &Interval) -> Result<Value, ExecError> {
    let number = |value: i64| Value::Numeric(Decimal::from(value));
    let result = match field {
        "year" => number((interval.months / 12) as i64),
        "month" => number((interval.months % 12) as i64),
        "day" => number(interval.days as i64),
        "hour" => number(interval.micros / 3_600_000_000),
        "minute" => number((interval.micros / 60_000_000) % 60),
        "second" => Value::Numeric(
            Decimal::from(interval.micros / 1_000_000 % 60)
                + Decimal::from(interval.micros % 1_000_000) / Decimal::from(1_000_000),
        ),
        "epoch" => number(interval.total_micros() / 1_000_000),
        other => {
            return Err(ExecError::other(format!(
                "unit \"{other}\" not supported for type interval"
            )))
        }
    };
    Ok(result)
}
