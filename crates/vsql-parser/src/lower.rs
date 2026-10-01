//! Lowering from `sqlparser-rs`'s dialect-neutral AST into VelocitySQL's AST.
//!
//! Every construct the engine does not implement is rejected here with a
//! `feature not supported` error. Nothing is silently dropped: a clause that
//! arrives from the parser and has no representation in [`crate::ast`] stops
//! the statement right away.

use rust_decimal::Decimal;
use sqlparser::ast as sa;

use vsql_types::{DataType, Interval, Value};

use crate::ast::*;
use crate::error::ParserError;

type Result<T> = std::result::Result<T, ParserError>;

/// Lowers statements, remembering which were spelled `CREATE USER`.
///
/// PostgreSQL's `CREATE USER` is `CREATE ROLE` with `LOGIN` defaulting to true,
/// so the flag travels with the statement rather than being guessed from the
/// text a second time.
pub fn lower_statements_with(
    statements: &[sa::Statement],
    from_create_user: &[bool],
) -> Result<Vec<Statement>> {
    statements
        .iter()
        .enumerate()
        .map(|(index, statement)| {
            lower_statement_with(
                statement,
                from_create_user.get(index).copied().unwrap_or(false),
            )
        })
        .collect()
}

/// Lowers a single statement.
pub fn lower_statement(statement: &sa::Statement) -> Result<Statement> {
    lower_statement_with(statement, false)
}

/// Lowers a single `sqlparser` expression; used by `parse_expression`.
pub fn lower_expr_public(expr: &sa::Expr) -> Result<Expr> {
    lower_expr(expr)
}

// ---------------------------------------------------------------- identifiers

/// Normalises an identifier the way PostgreSQL does.
///
/// Unquoted identifiers fold to lower case; quoted ones keep their spelling.
/// Discarding quote style here means every later stage can compare plain
/// `String`s, and `SELECT "Foo"` correctly fails to match `foo`.
fn ident(id: &sa::Ident) -> String {
    if id.quote_style.is_some() {
        id.value.clone()
    } else {
        id.value.to_ascii_lowercase()
    }
}

fn object_name(name: &sa::ObjectName) -> Result<Name> {
    let mut out = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        match part {
            sa::ObjectNamePart::Identifier(id) => out.push(ident(id)),
            sa::ObjectNamePart::Function(_) => {
                return Err(ParserError::NotSupported(
                    "dynamically computed object names".into(),
                ))
            }
        }
    }
    Ok(out)
}

/// Lowers a `PASSWORD` clause.
fn lower_password(password: &sa::Password) -> Result<RoleOption> {
    match password {
        sa::Password::NullPassword => Ok(RoleOption::Password(None)),
        sa::Password::Password(expr) => match expr {
            sa::Expr::Value(value) => match &value.value {
                sa::Value::SingleQuotedString(text)
                | sa::Value::DoubleQuotedString(text)
                | sa::Value::EscapedStringLiteral(text) => {
                    Ok(RoleOption::Password(Some(text.clone())))
                }
                other => Err(ParserError::NotSupported(format!(
                    "password literal `{other}`"
                ))),
            },
            other => Err(ParserError::NotSupported(format!(
                "password expression `{other}`"
            ))),
        },
    }
}

/// Lowers one attribute of `CREATE ROLE` / `ALTER ROLE`.
fn lower_role_option(option: &sa::RoleOption) -> Result<RoleOption> {
    match option {
        sa::RoleOption::SuperUser(value) => Ok(RoleOption::Superuser(*value)),
        sa::RoleOption::Login(value) => Ok(RoleOption::Login(*value)),
        sa::RoleOption::CreateDB(value) => Ok(RoleOption::CreateDb(*value)),
        sa::RoleOption::CreateRole(value) => Ok(RoleOption::CreateRole(*value)),
        sa::RoleOption::Password(password) => lower_password(password),
        other => Err(ParserError::NotSupported(format!("role option `{other}`"))),
    }
}

/// Resolves an object name that must be a single identifier, e.g. a database.
fn simple_name(name: &sa::ObjectName) -> Result<String> {
    let parts = object_name(name)?;
    match parts.as_slice() {
        [only] => Ok(only.clone()),
        _ => Err(ParserError::Invalid(format!(
            "`{name}` must be a single identifier"
        ))),
    }
}

fn column_name(name: &sa::ObjectName) -> Result<String> {
    let parts = object_name(name)?;
    parts
        .last()
        .cloned()
        .ok_or_else(|| ParserError::Invalid("empty column name".into()))
}

// ------------------------------------------------------------------- literals

fn lower_value(value: &sa::Value) -> Result<Value> {
    let lowered = match value {
        sa::Value::Number(text, _) => lower_number(text)?,
        sa::Value::Boolean(b) => Value::Bool(*b),
        sa::Value::Null => Value::Null,
        sa::Value::DollarQuotedString(s) => Value::Text(s.value.clone()),
        sa::Value::HexStringLiteral(s) => Value::Bytes(decode_hex(s)?),
        sa::Value::SingleQuotedByteStringLiteral(s)
        | sa::Value::DoubleQuotedByteStringLiteral(s)
        | sa::Value::TripleSingleQuotedByteStringLiteral(s)
        | sa::Value::TripleDoubleQuotedByteStringLiteral(s) => Value::Bytes(s.clone().into_bytes()),
        sa::Value::Placeholder(name) => {
            return Err(ParserError::NotSupported(format!(
                "parameter placeholders (`{name}`)"
            )))
        }
        sa::Value::SingleQuotedString(s)
        | sa::Value::DoubleQuotedString(s)
        | sa::Value::TripleSingleQuotedString(s)
        | sa::Value::TripleDoubleQuotedString(s)
        | sa::Value::SingleQuotedRawStringLiteral(s)
        | sa::Value::DoubleQuotedRawStringLiteral(s)
        | sa::Value::TripleSingleQuotedRawStringLiteral(s)
        | sa::Value::TripleDoubleQuotedRawStringLiteral(s)
        | sa::Value::EscapedStringLiteral(s)
        | sa::Value::UnicodeStringLiteral(s)
        | sa::Value::NationalStringLiteral(s) => Value::Text(s.clone()),
        other => return Err(ParserError::NotSupported(format!("literal `{other}`"))),
    };
    Ok(lowered)
}

/// Numeric literals are typed by their spelling, exactly like PostgreSQL:
/// `1` is `integer` (or `bigint`/`numeric` when it does not fit), `1.5` is
/// `numeric`.
fn lower_number(text: &str) -> Result<Value> {
    let cleaned = text.trim_end_matches(['L', 'l']);
    if cleaned.contains('.') || cleaned.contains('e') || cleaned.contains('E') {
        return Decimal::from_str_exact(cleaned)
            .or_else(|_| Decimal::from_scientific(cleaned))
            .map(Value::Numeric)
            .map_err(|_| ParserError::Invalid(format!("invalid numeric literal `{text}`")));
    }
    if let Ok(v) = cleaned.parse::<i32>() {
        return Ok(Value::Int4(v));
    }
    if let Ok(v) = cleaned.parse::<i64>() {
        return Ok(Value::Int8(v));
    }
    Decimal::from_str_exact(cleaned)
        .map(Value::Numeric)
        .map_err(|_| ParserError::Invalid(format!("invalid numeric literal `{text}`")))
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    if text.len() % 2 != 0 {
        return Err(ParserError::Invalid(format!(
            "invalid hexadecimal literal `{text}`"
        )));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| ParserError::Invalid(format!("invalid hexadecimal literal `{text}`")))?;
        let lo = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| ParserError::Invalid(format!("invalid hexadecimal literal `{text}`")))?;
        out.push((hi << 4 | lo) as u8);
    }
    Ok(out)
}

// ---------------------------------------------------------------- data types

fn character_length(len: &Option<sa::CharacterLength>) -> Option<u32> {
    match len {
        None => None,
        Some(sa::CharacterLength::IntegerLength { length, .. }) => Some(*length as u32),
        Some(sa::CharacterLength::Max) => None,
    }
}

fn exact_number(info: &sa::ExactNumberInfo) -> (Option<u32>, Option<u32>) {
    match info {
        sa::ExactNumberInfo::None => (None, None),
        sa::ExactNumberInfo::Precision(p) => (Some(*p as u32), None),
        sa::ExactNumberInfo::PrecisionAndScale(p, s) => (Some(*p as u32), Some((*s).max(0) as u32)),
    }
}

/// Maps a `sqlparser` type onto VelocitySQL's [`DataType`].
fn lower_data_type(ty: &sa::DataType) -> Result<DataType> {
    let lowered = match ty {
        sa::DataType::Bool | sa::DataType::Boolean => DataType::Bool,
        sa::DataType::Int2(_) | sa::DataType::SmallInt(_) => DataType::Int2,
        sa::DataType::Int(_) | sa::DataType::Int4(_) | sa::DataType::Integer(_) => DataType::Int4,
        sa::DataType::Int8(_) | sa::DataType::BigInt(_) => DataType::Int8,
        sa::DataType::Real | sa::DataType::Float4 | sa::DataType::Float32 => DataType::Float4,
        sa::DataType::Float8 | sa::DataType::Float64 => DataType::Float8,
        sa::DataType::Float(info) => match exact_number(info) {
            // `FLOAT(1..24)` is `real`, `FLOAT(25..53)` is `double precision`.
            (Some(p), _) if p <= 24 => DataType::Float4,
            _ => DataType::Float8,
        },
        sa::DataType::Double(_) | sa::DataType::DoublePrecision => DataType::Float8,
        sa::DataType::Numeric(info) | sa::DataType::Decimal(info) | sa::DataType::Dec(info) => {
            let (precision, scale) = exact_number(info);
            DataType::Numeric { precision, scale }
        }
        sa::DataType::Text
        | sa::DataType::String(_)
        | sa::DataType::TinyText
        | sa::DataType::MediumText
        | sa::DataType::LongText
        | sa::DataType::Regclass => DataType::Text,
        sa::DataType::Varchar(len)
        | sa::DataType::CharacterVarying(len)
        | sa::DataType::CharVarying(len) => DataType::Varchar(character_length(len)),
        sa::DataType::Char(len) | sa::DataType::Character(len) => {
            DataType::Char(character_length(len))
        }
        sa::DataType::Bytea
        | sa::DataType::Blob(_)
        | sa::DataType::Bytes(_)
        | sa::DataType::Varbinary(_)
        | sa::DataType::Binary(_)
        | sa::DataType::Bit(_)
        | sa::DataType::BitVarying(_)
        | sa::DataType::VarBit(_) => DataType::Bytea,
        sa::DataType::Date => DataType::Date,
        sa::DataType::Time(_, tz) => match tz {
            sa::TimezoneInfo::WithTimeZone | sa::TimezoneInfo::Tz => DataType::Timestamptz,
            _ => DataType::Time,
        },
        sa::DataType::Timestamp(_, tz) => match tz {
            sa::TimezoneInfo::WithTimeZone | sa::TimezoneInfo::Tz => DataType::Timestamptz,
            _ => DataType::Timestamp,
        },
        sa::DataType::Datetime(_) | sa::DataType::TimestampNtz(_) => DataType::Timestamp,
        sa::DataType::Interval { .. } => DataType::Interval,
        sa::DataType::Uuid => DataType::Uuid,
        sa::DataType::JSON => DataType::Json,
        sa::DataType::JSONB => DataType::Jsonb,
        sa::DataType::Array(elem) => {
            let inner = array_element(elem)?;
            DataType::Array(Box::new(inner))
        }
        sa::DataType::Custom(name, _) => {
            let text = object_name(name)?.join(".");
            match text.as_str() {
                "serial" | "serial4" => DataType::Int4,
                "bigserial" | "serial8" => DataType::Int8,
                "smallserial" | "serial2" => DataType::Int2,
                "timestamptz" => DataType::Timestamptz,
                "text" | "varchar" | "character varying" => DataType::Text,
                "bytea" => DataType::Bytea,
                "uuid" => DataType::Uuid,
                "jsonb" => DataType::Jsonb,
                "json" => DataType::Json,
                other => {
                    // Pseudo-types appear in tool queries under qualified or bare
                    // names: the SQL standard's `information_schema.*` family and
                    // PostgreSQL's object-identifier family (`CAST(x AS oid)`,
                    // `::pg_catalog.regclass`). They map onto ordinary types --
                    // the oid family is an unsigned 4-byte integer, `int4` here.
                    let standard = other
                        .strip_prefix("pg_catalog.")
                        .or_else(|| other.strip_prefix("information_schema."))
                        .unwrap_or(other);
                    match standard {
                        "oid" | "xid" | "cid" | "regclass" | "regproc" | "regprocedure"
                        | "regoper" | "regoperator" | "regnamespace" | "regrole" | "regtype"
                        | "regconfig" | "regdictionary" | "regcollation" => DataType::Int4,
                        "name" | "character_data" | "sql_identifier" | "yes_or_no" => {
                            DataType::Text
                        }
                        "cardinal_number" => DataType::Int4,
                        "time_stamp" => DataType::Timestamptz,
                        _ => match DataType::from_name(standard) {
                            Some(ty) => ty,
                            None => {
                                return Err(ParserError::NotSupported(format!(
                                    "data type `{other}`"
                                )))
                            }
                        },
                    }
                }
            }
        }
        other => return Err(ParserError::NotSupported(format!("data type `{other}`"))),
    };
    Ok(lowered)
}

fn array_element(elem: &sa::ArrayElemTypeDef) -> Result<DataType> {
    match elem {
        sa::ArrayElemTypeDef::AngleBracket(inner)
        | sa::ArrayElemTypeDef::SquareBracket(inner, _)
        | sa::ArrayElemTypeDef::Parenthesis(inner)
        | sa::ArrayElemTypeDef::Qualified(inner, _) => lower_data_type(inner),
        sa::ArrayElemTypeDef::None => Ok(DataType::Text),
    }
}

// --------------------------------------------------------------- expressions

fn lower_expr(expr: &sa::Expr) -> Result<Expr> {
    let lowered = match expr {
        sa::Expr::Identifier(id) => Expr::Column(ColumnRef {
            qualifier: None,
            name: ident(id),
            quoted: id.quote_style.is_some(),
        }),
        sa::Expr::CompoundIdentifier(parts) => {
            let mut names = Vec::with_capacity(parts.len());
            for part in parts {
                names.push(ident(part));
            }
            let name = names.pop().unwrap_or_default();
            let qualifier = if names.is_empty() {
                None
            } else {
                Some(names.join("."))
            };
            let quoted = parts
                .last()
                .map(|part| part.quote_style.is_some())
                .unwrap_or(false);
            Expr::Column(ColumnRef {
                qualifier,
                name,
                quoted,
            })
        }
        sa::Expr::Value(v) => match &v.value {
            sa::Value::Placeholder(name) => {
                let index = if let Some(rest) = name.strip_prefix('$') {
                    rest.parse::<usize>().map_err(|_| {
                        ParserError::NotSupported(format!("parameter placeholders (`{name}`)"))
                    })?
                } else if name == "?" {
                    0
                } else {
                    return Err(ParserError::NotSupported(format!(
                        "parameter placeholders (`{name}`)"
                    )));
                };
                Expr::Parameter(index.saturating_sub(1))
            }
            other => Expr::Literal(lower_value(other)?),
        },
        sa::Expr::TypedString(ts) => Expr::Cast {
            expr: Box::new(Expr::Literal(lower_value(&ts.value.value)?)),
            data_type: lower_data_type(&ts.data_type)?,
            try_cast: false,
        },
        sa::Expr::Interval(iv) => lower_interval(iv)?,
        sa::Expr::Nested(inner) => Expr::Nested(Box::new(lower_expr(inner)?)),
        sa::Expr::UnaryOp { op, expr } => Expr::Unary {
            op: lower_unary_op(op)?,
            expr: Box::new(lower_expr(expr)?),
        },
        sa::Expr::BinaryOp { left, op, right } => Expr::Binary {
            left: Box::new(lower_expr(left)?),
            op: lower_binary_op(op)?,
            right: Box::new(lower_expr(right)?),
        },
        sa::Expr::IsNull(inner) => Expr::IsNull {
            expr: Box::new(lower_expr(inner)?),
            negated: false,
        },
        sa::Expr::IsNotNull(inner) => Expr::IsNull {
            expr: Box::new(lower_expr(inner)?),
            negated: true,
        },
        sa::Expr::IsTrue(inner) => lower_is_bool(inner, Some(true), false)?,
        sa::Expr::IsNotTrue(inner) => lower_is_bool(inner, Some(true), true)?,
        sa::Expr::IsFalse(inner) => lower_is_bool(inner, Some(false), false)?,
        sa::Expr::IsNotFalse(inner) => lower_is_bool(inner, Some(false), true)?,
        sa::Expr::IsUnknown(inner) => lower_is_bool(inner, None, false)?,
        sa::Expr::IsNotUnknown(inner) => lower_is_bool(inner, None, true)?,
        sa::Expr::IsDistinctFrom(a, b) => Expr::Binary {
            left: Box::new(lower_expr(a)?),
            op: BinaryOp::IsDistinctFrom,
            right: Box::new(lower_expr(b)?),
        },
        sa::Expr::IsNotDistinctFrom(a, b) => Expr::Binary {
            left: Box::new(lower_expr(a)?),
            op: BinaryOp::IsNotDistinctFrom,
            right: Box::new(lower_expr(b)?),
        },
        sa::Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(lower_expr(expr)?),
            list: list.iter().map(lower_expr).collect::<Result<Vec<_>>>()?,
            negated: *negated,
        },
        sa::Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(lower_expr(expr)?),
            subquery: Box::new(lower_query(subquery)?),
            negated: *negated,
        },
        sa::Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(lower_expr(expr)?),
            negated: *negated,
            low: Box::new(lower_expr(low)?),
            high: Box::new(lower_expr(high)?),
        },
        sa::Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
            any: _,
        } => Expr::Like {
            expr: Box::new(lower_expr(expr)?),
            pattern: Box::new(lower_expr(pattern)?),
            negated: *negated,
            case_insensitive: false,
            escape: lower_escape(escape_char)?,
        },
        sa::Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
            any: _,
        } => Expr::Like {
            expr: Box::new(lower_expr(expr)?),
            pattern: Box::new(lower_expr(pattern)?),
            negated: *negated,
            case_insensitive: true,
            escape: lower_escape(escape_char)?,
        },
        sa::Expr::Cast {
            expr, data_type, ..
        } => Expr::Cast {
            expr: Box::new(lower_expr(expr)?),
            data_type: lower_data_type(data_type)?,
            try_cast: false,
        },
        sa::Expr::Function(f) => Expr::Function(lower_function(f)?),
        sa::Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let operand = match operand {
                Some(o) => Some(Box::new(lower_expr(o)?)),
                None => None,
            };
            let mut whens = Vec::with_capacity(conditions.len());
            for cw in conditions {
                whens.push((lower_expr(&cw.condition)?, lower_expr(&cw.result)?));
            }
            let else_result = match else_result {
                Some(e) => Some(Box::new(lower_expr(e)?)),
                None => None,
            };
            Expr::Case {
                operand,
                whens,
                else_result,
            }
        }
        sa::Expr::Subquery(q) => Expr::Subquery(Box::new(lower_query(q)?)),
        sa::Expr::Exists { subquery, negated } => Expr::Exists {
            subquery: Box::new(lower_query(subquery)?),
            negated: *negated,
        },
        sa::Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => Expr::AnyAll {
            left: Box::new(lower_expr(left)?),
            op: lower_binary_op(compare_op)?,
            right: Box::new(lower_expr(right)?),
            is_all: false,
        },
        sa::Expr::AllOp {
            left,
            compare_op,
            right,
        } => Expr::AnyAll {
            left: Box::new(lower_expr(left)?),
            op: lower_binary_op(compare_op)?,
            right: Box::new(lower_expr(right)?),
            is_all: true,
        },
        sa::Expr::Array(array) => Expr::Array(
            array
                .elem
                .iter()
                .map(lower_expr)
                .collect::<Result<Vec<_>>>()?,
        ),
        sa::Expr::Tuple(items) => {
            Expr::Row(items.iter().map(lower_expr).collect::<Result<Vec<_>>>()?)
        }
        sa::Expr::CompoundFieldAccess { root, access_chain } => {
            // Only array subscripts and row-type field access are supported;
            // fold the chain left to right, so `a[1][2]` becomes `(a[1])[2]`.
            let mut expr = lower_expr(root)?;
            for access in access_chain {
                match access {
                    sa::AccessExpr::Subscript(sa::Subscript::Index { index }) => {
                        expr = Expr::Subscript {
                            expr: Box::new(expr),
                            index: Box::new(lower_expr(index)?),
                        };
                    }
                    sa::AccessExpr::Dot(field) => {
                        // `(alias).column` is a parenthesised spelling of
                        // `alias.column`: the parentheses keep the parser
                        // from forming a compound identifier, so rebuild the
                        // qualified column here.
                        let base = match expr {
                            Expr::Nested(inner) => *inner,
                            other => other,
                        };
                        let field = lower_expr(field)?;
                        match (base, field) {
                            (Expr::Column(mut reference), Expr::Column(field_reference))
                                if reference.qualifier.is_none() =>
                            {
                                reference.qualifier = Some(reference.name);
                                reference.name = field_reference.name;
                                expr = Expr::Column(reference);
                            }
                            (base, field) => {
                                let _ = base;
                                let field_name = match field {
                                    Expr::Column(reference) => reference.name,
                                    _ => "?".to_string(),
                                };
                                return Err(ParserError::NotSupported(format!(
                                    "field access `.{field_name}`"
                                )));
                            }
                        }
                    }
                    other => {
                        return Err(ParserError::NotSupported(format!("field access `{other}`")))
                    }
                }
            }
            expr
        }
        sa::Expr::Wildcard(_) => Expr::Wildcard,
        sa::Expr::QualifiedWildcard(name, _) => {
            Expr::QualifiedWildcard(object_name(name)?.join("."))
        }
        sa::Expr::Extract { field, expr, .. } => Expr::Function(FunctionCall {
            name: "extract".to_string(),
            args: vec![
                Expr::Literal(Value::Text(datetime_field(field)?.to_string())),
                lower_expr(expr)?,
            ],
            distinct: false,
            star: false,
            filter: None,
            order_by: Vec::new(),
        }),
        sa::Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            let mut args = vec![lower_expr(expr)?];
            if let Some(from) = substring_from {
                args.push(lower_expr(from)?);
            }
            if let Some(len) = substring_for {
                args.push(lower_expr(len)?);
            }
            Expr::Function(FunctionCall {
                name: "substring".to_string(),
                args,
                distinct: false,
                star: false,
                filter: None,
                order_by: Vec::new(),
            })
        }
        sa::Expr::Trim {
            expr, trim_where, ..
        } => {
            let name = match trim_where {
                Some(sa::TrimWhereField::Leading) => "ltrim",
                Some(sa::TrimWhereField::Trailing) => "rtrim",
                _ => "btrim",
            };
            Expr::Function(FunctionCall {
                name: name.to_string(),
                args: vec![lower_expr(expr)?],
                distinct: false,
                star: false,
                filter: None,
                order_by: Vec::new(),
            })
        }
        other => return Err(ParserError::NotSupported(format!("expression `{other}`"))),
    };
    Ok(lowered)
}

fn lower_escape(escape: &Option<Box<sa::Expr>>) -> Result<Option<Box<Expr>>> {
    match escape {
        None => Ok(None),
        Some(e) => Ok(Some(Box::new(lower_expr(e)?))),
    }
}

fn lower_is_bool(expr: &sa::Expr, value: Option<bool>, negated: bool) -> Result<Expr> {
    Ok(Expr::IsBool {
        expr: Box::new(lower_expr(expr)?),
        value,
        negated,
    })
}

fn lower_function(f: &sa::Function) -> Result<FunctionCall> {
    if f.over.is_some() {
        return Err(ParserError::NotSupported(
            "window functions (`OVER`) are planned for milestone M5".into(),
        ));
    }
    if !f.within_group.is_empty() {
        return Err(ParserError::NotSupported("WITHIN GROUP".into()));
    }
    let mut call = FunctionCall {
        name: object_name(&f.name)?.join(".").to_ascii_lowercase(),
        args: Vec::new(),
        distinct: false,
        star: false,
        filter: match &f.filter {
            Some(pred) => Some(Box::new(lower_expr(pred)?)),
            None => None,
        },
        order_by: Vec::new(),
    };
    match &f.args {
        sa::FunctionArguments::None => {}
        sa::FunctionArguments::Subquery(q) => {
            call.args.push(Expr::Subquery(Box::new(lower_query(q)?)));
        }
        sa::FunctionArguments::List(list) => {
            call.distinct = matches!(
                list.duplicate_treatment,
                Some(sa::DuplicateTreatment::Distinct)
            );
            for clause in &list.clauses {
                match clause {
                    // `agg(x ORDER BY y)`: recorded here and dropped by the
                    // planner, which knows whether the called aggregate
                    // observes the order of its inputs.
                    sa::FunctionArgumentClause::OrderBy(items) => {
                        call.order_by = items
                            .iter()
                            .map(lower_order_by)
                            .collect::<Result<Vec<_>>>()?;
                    }
                    other => {
                        return Err(ParserError::NotSupported(format!(
                            "clause `{other}` in a function argument list"
                        )))
                    }
                }
            }
            for arg in &list.args {
                match arg {
                    sa::FunctionArg::Unnamed(sa::FunctionArgExpr::Wildcard) => call.star = true,
                    sa::FunctionArg::Unnamed(sa::FunctionArgExpr::Expr(e)) => {
                        call.args.push(lower_expr(e)?)
                    }
                    sa::FunctionArg::Named { arg, .. } | sa::FunctionArg::ExprNamed { arg, .. } => {
                        match arg {
                            sa::FunctionArgExpr::Expr(e) => call.args.push(lower_expr(e)?),
                            sa::FunctionArgExpr::Wildcard => call.star = true,
                            other => {
                                return Err(ParserError::NotSupported(format!(
                                    "function argument `{other}`"
                                )))
                            }
                        }
                    }
                    other => {
                        return Err(ParserError::NotSupported(format!(
                            "function argument `{other}`"
                        )))
                    }
                }
            }
        }
    }
    Ok(call)
}

fn datetime_field(field: &sa::DateTimeField) -> Result<&'static str> {
    use sa::DateTimeField as F;
    let name = match field {
        F::Year | F::Years => "year",
        F::Month | F::Months => "month",
        F::Week(_) | F::Weeks => "week",
        F::Day | F::Days => "day",
        F::Hour | F::Hours => "hour",
        F::Minute | F::Minutes => "minute",
        F::Second | F::Seconds => "second",
        F::Millisecond => "millisecond",
        F::Microsecond => "microsecond",
        F::Quarter => "quarter",
        F::Epoch => "epoch",
        F::DayOfYear => "doy",
        F::DayOfWeek => "dow",

        F::Century => "century",
        F::Decade => "decade",
        other => {
            return Err(ParserError::NotSupported(format!(
                "EXTRACT field `{other}`"
            )))
        }
    };
    Ok(name)
}

fn lower_interval(iv: &sa::Interval) -> Result<Expr> {
    let text = match &*iv.value {
        sa::Expr::Value(v) => match lower_value(&v.value)? {
            Value::Text(s) => s,
            Value::Int4(v) => v.to_string(),
            Value::Int8(v) => v.to_string(),
            Value::Numeric(v) => v.to_string(),
            other => {
                return Err(ParserError::Invalid(format!(
                    "invalid interval literal `{other}`"
                )))
            }
        },
        other => {
            return Err(ParserError::NotSupported(format!(
                "non-constant interval value `{other}`"
            )));
        }
    };
    let full = match &iv.leading_field {
        Some(field) => format!("{text} {}", datetime_field(field)?),
        None => text,
    };
    let parsed = Interval::parse(&full).map_err(|e| ParserError::Invalid(e.to_string()))?;
    Ok(Expr::Literal(Value::Interval(parsed)))
}

fn lower_binary_op(op: &sa::BinaryOperator) -> Result<BinaryOp> {
    use sa::BinaryOperator as B;
    let lowered = match op {
        B::Plus => BinaryOp::Plus,
        B::Minus => BinaryOp::Minus,
        B::Multiply => BinaryOp::Multiply,
        B::Divide => BinaryOp::Divide,
        B::Modulo => BinaryOp::Modulo,
        B::StringConcat => BinaryOp::Concat,
        B::Eq => BinaryOp::Eq,
        B::NotEq => BinaryOp::NotEq,
        B::Lt => BinaryOp::Lt,
        B::LtEq => BinaryOp::LtEq,
        B::Gt => BinaryOp::Gt,
        B::GtEq => BinaryOp::GtEq,
        B::And => BinaryOp::And,
        B::Or => BinaryOp::Or,

        B::BitwiseAnd => BinaryOp::BitAnd,
        B::BitwiseOr => BinaryOp::BitOr,
        B::BitwiseXor => BinaryOp::BitXor,
        B::PGBitwiseXor => BinaryOp::BitXor,
        B::PGBitwiseShiftLeft => BinaryOp::ShiftLeft,
        B::PGBitwiseShiftRight => BinaryOp::ShiftRight,
        B::PGExp => BinaryOp::Pow,
        B::PGRegexMatch => BinaryOp::RegexMatch,
        B::PGRegexIMatch => BinaryOp::RegexIMatch,
        B::PGRegexNotMatch => BinaryOp::RegexNotMatch,
        B::PGRegexNotIMatch => BinaryOp::RegexNotIMatch,
        other => return Err(ParserError::NotSupported(format!("operator `{other}`"))),
    };
    Ok(lowered)
}

fn lower_unary_op(op: &sa::UnaryOperator) -> Result<UnaryOp> {
    use sa::UnaryOperator as U;
    let lowered = match op {
        U::Plus => UnaryOp::Plus,
        U::Minus => UnaryOp::Negate,
        U::Not | U::BangNot => UnaryOp::Not,
        U::BitwiseNot => UnaryOp::BitNot,
        other => return Err(ParserError::NotSupported(format!("operator `{other}`"))),
    };
    Ok(lowered)
}

// -------------------------------------------------------------------- queries

fn lower_query(query: &sa::Query) -> Result<Query> {
    if !query.locks.is_empty() {
        return Err(ParserError::NotSupported(
            "row locking clauses (`FOR UPDATE`/`FOR SHARE`)".into(),
        ));
    }
    if query.fetch.is_some() {
        return Err(ParserError::NotSupported("FETCH FIRST".into()));
    }
    let with = match &query.with {
        None => None,
        Some(w) => {
            let mut ctes = Vec::with_capacity(w.cte_tables.len());
            for cte in &w.cte_tables {
                ctes.push(Cte {
                    name: ident(&cte.alias.name),
                    columns: cte.alias.columns.iter().map(|c| ident(&c.name)).collect(),
                    query: lower_query(&cte.query)?,
                });
            }
            Some(With {
                recursive: w.recursive,
                ctes,
            })
        }
    };

    let (limit, offset) = match &query.limit_clause {
        None => (None, None),
        Some(sa::LimitClause::LimitOffset { limit, offset, .. }) => {
            let offset = match offset {
                Some(o) => Some(lower_expr(&o.value)?),
                None => None,
            };
            let limit = match limit {
                Some(l) => Some(lower_expr(l)?),
                None => None,
            };
            (limit, offset)
        }
        Some(sa::LimitClause::OffsetCommaLimit { offset, limit }) => {
            (Some(lower_expr(limit)?), Some(lower_expr(offset)?))
        }
    };

    let order_by = match &query.order_by {
        None => Vec::new(),
        Some(ob) => match &ob.kind {
            sa::OrderByKind::Expressions(exprs) => exprs
                .iter()
                .map(lower_order_by)
                .collect::<Result<Vec<_>>>()?,
            sa::OrderByKind::All(_) => {
                return Err(ParserError::NotSupported("ORDER BY ALL".into()))
            }
        },
    };

    Ok(Query {
        with,
        body: lower_set_expr(&query.body)?,
        order_by,
        limit,
        offset,
        for_update: false,
    })
}

fn lower_order_by(expr: &sa::OrderByExpr) -> Result<OrderByExpr> {
    if expr.with_fill.is_some() {
        return Err(ParserError::NotSupported("WITH FILL".into()));
    }
    let asc = !matches!(expr.options.sort, Some(sa::OrderBySort::Desc));
    if matches!(expr.options.sort, Some(sa::OrderBySort::Using(_))) {
        return Err(ParserError::NotSupported("ORDER BY ... USING".into()));
    }
    Ok(OrderByExpr {
        expr: lower_expr(&expr.expr)?,
        asc,
        nulls_first: expr.options.nulls_first,
    })
}

fn lower_set_expr(body: &sa::SetExpr) -> Result<SetExpr> {
    let lowered = match body {
        sa::SetExpr::Select(select) => SetExpr::Select(Box::new(lower_select(select)?)),
        sa::SetExpr::Query(query) => SetExpr::Query(Box::new(lower_query(query)?)),
        sa::SetExpr::Values(values) => SetExpr::Values(
            values
                .rows
                .iter()
                .map(|row| {
                    row.content
                        .iter()
                        .map(lower_expr)
                        .collect::<Result<Vec<_>>>()
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        sa::SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => SetExpr::SetOperation {
            op: match op {
                sa::SetOperator::Union => SetOp::Union,
                sa::SetOperator::Intersect => SetOp::Intersect,
                sa::SetOperator::Except | sa::SetOperator::Minus => SetOp::Except,
            },
            all: matches!(set_quantifier, sa::SetQuantifier::All),
            left: Box::new(lower_set_expr(left)?),
            right: Box::new(lower_set_expr(right)?),
        },
        other => return Err(ParserError::NotSupported(format!("query body `{other}`"))),
    };
    Ok(lowered)
}

fn lower_select(select: &sa::Select) -> Result<Select> {
    if !select.named_window.is_empty() || select.qualify.is_some() {
        return Err(ParserError::NotSupported("WINDOW / QUALIFY clauses".into()));
    }
    if !select.lateral_views.is_empty() || !select.cluster_by.is_empty() {
        return Err(ParserError::NotSupported(
            "lateral views / CLUSTER BY".into(),
        ));
    }
    if select.top.is_some() {
        return Err(ParserError::NotSupported("TOP".into()));
    }
    if select.into.is_some() {
        return Err(ParserError::NotSupported("SELECT INTO".into()));
    }
    let distinct = match &select.distinct {
        None => None,
        Some(sa::Distinct::All) => None,
        Some(sa::Distinct::Distinct) => Some(Distinct::On),
        Some(sa::Distinct::On(exprs)) => Some(Distinct::OnExprs(
            exprs.iter().map(lower_expr).collect::<Result<Vec<_>>>()?,
        )),
    };
    let projection = select
        .projection
        .iter()
        .map(lower_select_item)
        .collect::<Result<Vec<_>>>()?;
    let from = select
        .from
        .iter()
        .map(lower_table_with_joins)
        .collect::<Result<Vec<_>>>()?;
    let selection = match &select.selection {
        Some(e) => Some(lower_expr(e)?),
        None => None,
    };
    let group_by = match &select.group_by {
        sa::GroupByExpr::Expressions(exprs, _) => {
            exprs.iter().map(lower_expr).collect::<Result<Vec<_>>>()?
        }
        sa::GroupByExpr::All(_) => return Err(ParserError::NotSupported("GROUP BY ALL".into())),
    };
    let having = match &select.having {
        Some(e) => Some(lower_expr(e)?),
        None => None,
    };
    Ok(Select {
        distinct,
        projection,
        from,
        selection,
        group_by,
        having,
    })
}

fn lower_select_item(item: &sa::SelectItem) -> Result<SelectItem> {
    let lowered = match item {
        sa::SelectItem::UnnamedExpr(expr) => SelectItem::Expr {
            expr: lower_expr(expr)?,
            alias: None,
        },
        sa::SelectItem::ExprWithAlias { expr, alias } => SelectItem::Expr {
            expr: lower_expr(expr)?,
            alias: Some(ident(alias)),
        },
        sa::SelectItem::Wildcard(_) => SelectItem::Wildcard,
        sa::SelectItem::QualifiedWildcard(
            sa::SelectItemQualifiedWildcardKind::ObjectName(name),
            _,
        ) => SelectItem::QualifiedWildcard(object_name(name)?.join(".")),
        other => return Err(ParserError::NotSupported(format!("select item `{other}`"))),
    };
    Ok(lowered)
}

fn lower_table_with_joins(item: &sa::TableWithJoins) -> Result<TableWithJoins> {
    let relation = lower_table_factor(&item.relation)?;
    let mut joins = Vec::with_capacity(item.joins.len());
    for join in &item.joins {
        joins.push(lower_join(join)?);
    }
    Ok(TableWithJoins { relation, joins })
}

fn lower_join(join: &sa::Join) -> Result<Join> {
    let relation = lower_table_factor(&join.relation)?;
    let (op, constraint) = match &join.join_operator {
        sa::JoinOperator::Join(c) => (JoinType::Inner, lower_constraint(c)?),
        sa::JoinOperator::Inner(c) => (JoinType::Inner, lower_constraint(c)?),
        sa::JoinOperator::Left(c) | sa::JoinOperator::LeftOuter(c) => {
            (JoinType::Left, lower_constraint(c)?)
        }
        sa::JoinOperator::Right(c) | sa::JoinOperator::RightOuter(c) => {
            (JoinType::Right, lower_constraint(c)?)
        }
        sa::JoinOperator::FullOuter(c) => (JoinType::Full, lower_constraint(c)?),
        sa::JoinOperator::CrossJoin(c) => (JoinType::Cross, lower_constraint(c)?),
        other => return Err(ParserError::NotSupported(format!("join type `{other:?}`"))),
    };
    Ok(Join {
        relation,
        op,
        constraint,
    })
}

fn lower_constraint(constraint: &sa::JoinConstraint) -> Result<JoinConstraint> {
    let lowered = match constraint {
        sa::JoinConstraint::On(expr) => JoinConstraint::On(lower_expr(expr)?),
        sa::JoinConstraint::Using(names) => {
            JoinConstraint::Using(names.iter().map(column_name).collect::<Result<Vec<_>>>()?)
        }
        sa::JoinConstraint::Natural => JoinConstraint::Natural,
        sa::JoinConstraint::None => JoinConstraint::None,
    };
    Ok(lowered)
}

fn lower_table_factor(factor: &sa::TableFactor) -> Result<TableFactor> {
    let lowered = match factor {
        sa::TableFactor::Table {
            name,
            alias,
            args,
            with_ordinality,
            ..
        } => {
            if *with_ordinality {
                return Err(ParserError::NotSupported("WITH ORDINALITY".into()));
            }
            match args {
                // `FROM t` names a relation; `FROM f(...)` is a call, and a
                // zero-argument call such as `FROM pg_get_keywords()` is
                // still a call to a set-returning function.
                None => TableFactor::Table {
                    name: object_name(name)?,
                    alias: lower_alias(alias)?,
                },
                Some(args) => {
                    if args.settings.as_ref().is_some_and(|list| !list.is_empty()) {
                        return Err(ParserError::NotSupported(
                            "SETTINGS in a table function".into(),
                        ));
                    }
                    TableFactor::Function {
                        name: object_name(name)?,
                        args: lower_function_args(&args.args)?,
                        alias: lower_alias(alias)?,
                    }
                }
            }
        }
        sa::TableFactor::Derived {
            lateral,
            subquery,
            alias,
            ..
        } => TableFactor::Derived {
            subquery: Box::new(lower_query(subquery)?),
            alias: lower_alias(alias)?,
            lateral: *lateral,
        },
        sa::TableFactor::NestedJoin {
            table_with_joins,
            alias,
        } => TableFactor::NestedJoin {
            table: Box::new(lower_table_with_joins(table_with_joins)?),
            alias: lower_alias(alias)?,
        },
        sa::TableFactor::Function {
            lateral: _,
            name,
            args,
            with_ordinality,
            alias,
        } => {
            if *with_ordinality {
                return Err(ParserError::NotSupported("WITH ORDINALITY".into()));
            }
            TableFactor::Function {
                name: object_name(name)?,
                args: lower_function_args(args)?,
                alias: lower_alias(alias)?,
            }
        }
        sa::TableFactor::UNNEST {
            alias,
            array_exprs,
            with_offset,
            with_offset_alias: _,
            with_ordinality,
        } => {
            if *with_offset || *with_ordinality {
                return Err(ParserError::NotSupported(
                    "UNNEST modifiers (WITH OFFSET/WITH ORDINALITY)".into(),
                ));
            }
            // PostgreSQL zips several arrays into one row set; the engine
            // answers the single-array form, which is what clients use.
            let [array] = array_exprs.as_slice() else {
                return Err(ParserError::NotSupported(
                    "UNNEST over several arrays".into(),
                ));
            };
            TableFactor::Function {
                name: vec!["unnest".to_string()],
                args: vec![lower_expr(array)?],
                alias: lower_alias(alias)?,
            }
        }
        other => return Err(ParserError::NotSupported(format!("FROM item `{other}`"))),
    };
    Ok(lowered)
}

/// Lowers the arguments of a set-returning function used in `FROM`.
///
/// This is the same lowering a scalar call gets, minus the modifiers
/// (`DISTINCT`, `*`, `FILTER`) that make no sense for a table function.
fn lower_function_args(args: &[sa::FunctionArg]) -> Result<Vec<Expr>> {
    let mut lowered = Vec::new();
    for arg in args {
        match arg {
            sa::FunctionArg::Unnamed(sa::FunctionArgExpr::Expr(e)) => lowered.push(lower_expr(e)?),
            sa::FunctionArg::Named { arg, .. } | sa::FunctionArg::ExprNamed { arg, .. } => {
                match arg {
                    sa::FunctionArgExpr::Expr(e) => lowered.push(lower_expr(e)?),
                    other => {
                        return Err(ParserError::NotSupported(format!(
                            "function argument `{other}`"
                        )))
                    }
                }
            }
            other => {
                return Err(ParserError::NotSupported(format!(
                    "function argument `{other}`"
                )))
            }
        }
    }
    Ok(lowered)
}
fn lower_alias(alias: &Option<sa::TableAlias>) -> Result<Option<TableAlias>> {
    match alias {
        None => Ok(None),
        Some(a) => Ok(Some(TableAlias {
            name: ident(&a.name),
            columns: a.columns.iter().map(|c| ident(&c.name)).collect(),
        })),
    }
}

// ----------------------------------------------------------------- statements

fn lower_statement_with(statement: &sa::Statement, from_create_user: bool) -> Result<Statement> {
    let lowered = match statement {
        sa::Statement::Query(query) => Statement::Query(Box::new(lower_query(query)?)),
        sa::Statement::Insert(insert) => Statement::Insert(Box::new(lower_insert(insert)?)),
        sa::Statement::Update(update) => Statement::Update(Box::new(lower_update(update)?)),
        sa::Statement::Delete(delete) => Statement::Delete(Box::new(lower_delete(delete)?)),
        sa::Statement::CreateTable(create) => {
            Statement::CreateTable(Box::new(lower_create_table(create)?))
        }
        sa::Statement::CreateIndex(create) => {
            Statement::CreateIndex(Box::new(lower_create_index(create)?))
        }
        sa::Statement::AlterTable(alter) => {
            Statement::AlterTable(Box::new(lower_alter_table(alter)?))
        }
        sa::Statement::CreateView(view) => Statement::CreateView(Box::new(CreateView {
            name: object_name(&view.name)?,
            or_replace: view.or_replace,
            columns: view.columns.iter().map(|c| ident(&c.name)).collect(),
            query: Box::new(lower_query(&view.query)?),
        })),
        sa::Statement::CreateRole(create) => {
            // Reject the options we cannot honour instead of ignoring them.
            if create.inherit.is_some()
                || create.bypassrls.is_some()
                || create.replication.is_some()
                || create.connection_limit.is_some()
                || create.valid_until.is_some()
                || !create.in_role.is_empty()
                || !create.in_group.is_empty()
                || !create.role.is_empty()
                || !create.user.is_empty()
                || !create.admin.is_empty()
                || create.authorization_owner.is_some()
            {
                return Err(ParserError::NotSupported("CREATE ROLE options".into()));
            }
            let name = match create.names.as_slice() {
                [only] => simple_name(only)?,
                _ => {
                    return Err(ParserError::NotSupported(
                        "creating several roles in one statement".into(),
                    ))
                }
            };
            let mut options = Vec::new();
            if let Some(value) = create.superuser {
                options.push(RoleOption::Superuser(value));
            }
            // `CREATE USER` logs in by default; `CREATE ROLE` does not.
            let login = if from_create_user {
                Some(create.login.unwrap_or(true))
            } else {
                create.login
            };
            if let Some(value) = login {
                options.push(RoleOption::Login(value));
            }
            if let Some(value) = create.create_db {
                options.push(RoleOption::CreateDb(value));
            }
            if let Some(value) = create.create_role {
                options.push(RoleOption::CreateRole(value));
            }
            if let Some(password) = &create.password {
                options.push(lower_password(password)?);
            }
            Statement::CreateRole {
                name,
                if_not_exists: create.if_not_exists,
                options,
            }
        }
        sa::Statement::AlterRole { name, operation } => match operation {
            sa::AlterRoleOperation::WithOptions { options } => Statement::AlterRole {
                name: ident(name),
                options: options
                    .iter()
                    .map(lower_role_option)
                    .collect::<Result<Vec<_>>>()?,
            },
            other => {
                return Err(ParserError::NotSupported(format!(
                    "ALTER ROLE {}",
                    first_words(&other.to_string())
                )))
            }
        },
        sa::Statement::CreateDatabase {
            db_name,
            if_not_exists,
            location,
            managed_location,
            or_replace,
            transient,
            clone,
            ..
        } => {
            if location.is_some()
                || managed_location.is_some()
                || *or_replace
                || *transient
                || clone.is_some()
            {
                return Err(ParserError::NotSupported("CREATE DATABASE options".into()));
            }
            Statement::CreateDatabase {
                name: simple_name(db_name)?,
                if_not_exists: *if_not_exists,
            }
        }
        sa::Statement::CreateSchema {
            schema_name,
            if_not_exists,
            ..
        } => Statement::CreateSchema {
            name: match schema_name {
                sa::SchemaName::Simple(name) => column_name(name)?,
                other => {
                    return Err(ParserError::NotSupported(format!(
                        "CREATE SCHEMA `{other}`"
                    )))
                }
            },
            if_not_exists: *if_not_exists,
        },
        sa::Statement::Drop {
            object_type,
            if_exists,
            names,
            ..
        } => match object_type {
            sa::ObjectType::Table => Statement::DropTable {
                if_exists: *if_exists,
                names: names.iter().map(object_name).collect::<Result<Vec<_>>>()?,
            },
            sa::ObjectType::Database => Statement::DropDatabase {
                if_exists: *if_exists,
                names: names.iter().map(simple_name).collect::<Result<Vec<_>>>()?,
            },
            sa::ObjectType::Role | sa::ObjectType::User => Statement::DropRole {
                if_exists: *if_exists,
                names: names.iter().map(simple_name).collect::<Result<Vec<_>>>()?,
            },
            other => {
                return Err(ParserError::NotSupported(format!("DROP {other}")));
            }
        },
        sa::Statement::Truncate(truncate) => Statement::Truncate {
            names: truncate
                .table_names
                .iter()
                .map(|t| object_name(&t.name))
                .collect::<Result<Vec<_>>>()?,
            if_exists: truncate.if_exists,
        },
        sa::Statement::StartTransaction { modes, .. } => {
            let mut isolation = None;
            let mut read_only = None;
            for mode in modes {
                match mode {
                    sa::TransactionMode::IsolationLevel(level) => {
                        isolation = Some(match level {
                            sa::TransactionIsolationLevel::ReadUncommitted => {
                                IsolationLevel::ReadUncommitted
                            }
                            sa::TransactionIsolationLevel::ReadCommitted => {
                                IsolationLevel::ReadCommitted
                            }
                            sa::TransactionIsolationLevel::RepeatableRead => {
                                IsolationLevel::RepeatableRead
                            }
                            sa::TransactionIsolationLevel::Serializable => {
                                IsolationLevel::Serializable
                            }
                            // `SNAPSHOT` is not a PostgreSQL level; VelocitySQL
                            // maps it onto `REPEATABLE READ`, whose semantics match.
                            sa::TransactionIsolationLevel::Snapshot => {
                                IsolationLevel::RepeatableRead
                            }
                        });
                    }
                    sa::TransactionMode::AccessMode(mode) => {
                        read_only = Some(matches!(mode, sa::TransactionAccessMode::ReadOnly));
                    }
                }
            }
            Statement::StartTransaction {
                isolation,
                read_only,
            }
        }
        sa::Statement::Commit { .. } => Statement::Commit,
        sa::Statement::Rollback { savepoint, .. } => Statement::Rollback {
            savepoint: savepoint.as_ref().map(ident),
        },
        sa::Statement::Savepoint { name } => Statement::Savepoint(ident(name)),
        sa::Statement::ReleaseSavepoint { name } => Statement::ReleaseSavepoint(ident(name)),
        sa::Statement::Explain {
            analyze,
            verbose,
            statement,
            ..
        } => Statement::Explain {
            analyze: *analyze,
            verbose: *verbose,
            statement: Box::new(lower_statement(statement)?),
        },
        sa::Statement::ShowVariable { variable } => {
            Statement::Show(variable.iter().map(ident).collect::<Vec<_>>().join("."))
        }
        sa::Statement::Set(sa::Set::SingleAssignment {
            variable, values, ..
        }) => Statement::Set {
            name: object_name(variable)?.join(".").to_ascii_lowercase(),
            value: match values.first() {
                Some(v) => Some(lower_expr(v)?),
                None => None,
            },
        },
        // `SET TIME ZONE 'x'` is PostgreSQL's shorthand for
        // `SET timezone TO 'x'`, and clients use it as such.
        sa::Statement::Set(sa::Set::SetTimeZone { value, .. }) => Statement::Set {
            name: "timezone".to_string(),
            value: Some(lower_expr(value)?),
        },
        other => {
            return Err(ParserError::NotSupported(format!(
                "statement `{}`",
                first_words(&other.to_string())
            )))
        }
    };
    Ok(lowered)
}

/// Helper used only to keep the "unsupported statement" error short.
fn first_words(text: &str) -> String {
    text.split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

fn lower_insert(insert: &sa::Insert) -> Result<Insert> {
    if insert.or.is_some() || insert.ignore || insert.overwrite || insert.replace_into {
        return Err(ParserError::NotSupported(
            "INSERT conflict modifiers".into(),
        ));
    }
    if insert.on.is_some() {
        return Err(ParserError::NotSupported(
            "`ON CONFLICT`/`ON DUPLICATE KEY` is planned for milestone M4".into(),
        ));
    }
    let table = match &insert.table {
        sa::TableObject::TableName(name) => object_name(name)?,
        other => {
            return Err(ParserError::NotSupported(format!(
                "INSERT target `{other}`"
            )))
        }
    };
    let alias = insert.table_alias.as_ref().map(|alias| ident(&alias.alias));
    let columns = insert
        .columns
        .iter()
        .map(column_name)
        .collect::<Result<Vec<_>>>()?;
    let source = match (&insert.source, insert.assignments.is_empty()) {
        (Some(query), true) => match &*query.body {
            sa::SetExpr::Values(values) => InsertSource::Values(
                values
                    .rows
                    .iter()
                    .map(|row| {
                        row.content
                            .iter()
                            .map(lower_expr)
                            .collect::<Result<Vec<_>>>()
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            _ => InsertSource::Query(Box::new(lower_query(query)?)),
        },
        (None, true) => InsertSource::DefaultValues,
        _ => return Err(ParserError::NotSupported("INSERT ... SET".into())),
    };
    let returning = match &insert.returning {
        Some(items) => items
            .iter()
            .map(lower_select_item)
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    Ok(Insert {
        table,
        alias,
        columns,
        source,
        returning,
    })
}

fn lower_update(update: &sa::Update) -> Result<Update> {
    if update.from.is_some() {
        return Err(ParserError::NotSupported("UPDATE ... FROM".into()));
    }
    if !update.order_by.is_empty() || update.limit.is_some() {
        return Err(ParserError::NotSupported(
            "UPDATE ... ORDER BY/LIMIT".into(),
        ));
    }
    let (table, alias) = lower_single_table(&update.table)?;
    let mut assignments = Vec::with_capacity(update.assignments.len());
    for assignment in &update.assignments {
        let sa::AssignmentTarget::ColumnName(name) = &assignment.target else {
            return Err(ParserError::NotSupported(
                "tuple assignment in UPDATE".into(),
            ));
        };
        assignments.push((column_name(name)?, lower_expr(&assignment.value)?));
    }
    let selection = match &update.selection {
        Some(e) => Some(lower_expr(e)?),
        None => None,
    };
    let returning = match &update.returning {
        Some(items) => items
            .iter()
            .map(lower_select_item)
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    Ok(Update {
        table,
        alias,
        assignments,
        selection,
        returning,
    })
}

fn lower_delete(delete: &sa::Delete) -> Result<Delete> {
    if delete.using.is_some() {
        return Err(ParserError::NotSupported("DELETE ... USING".into()));
    }
    if !delete.tables.is_empty() {
        return Err(ParserError::NotSupported("multi-table DELETE".into()));
    }
    let tables = match &delete.from {
        sa::FromTable::WithFromKeyword(tables) | sa::FromTable::WithoutKeyword(tables) => tables,
    };
    if tables.len() != 1 {
        return Err(ParserError::NotSupported("multi-table DELETE".into()));
    }
    let (table, alias) = lower_single_table(&tables[0])?;
    let selection = match &delete.selection {
        Some(e) => Some(lower_expr(e)?),
        None => None,
    };
    let returning = match &delete.returning {
        Some(items) => items
            .iter()
            .map(lower_select_item)
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    Ok(Delete {
        table,
        alias,
        selection,
        returning,
    })
}

fn lower_single_table(item: &sa::TableWithJoins) -> Result<(Name, Option<String>)> {
    if !item.joins.is_empty() {
        return Err(ParserError::NotSupported("multi-table DML".into()));
    }
    match &item.relation {
        sa::TableFactor::Table { name, alias, .. } => {
            Ok((object_name(name)?, alias.as_ref().map(|a| ident(&a.name))))
        }
        other => Err(ParserError::NotSupported(format!("DML target `{other}`"))),
    }
}

fn lower_create_table(create: &sa::CreateTable) -> Result<CreateTable> {
    // VelocitySQL is always in memory, so `TEMP` and `UNLOGGED` are accepted as
    // no-ops rather than rejected. Anything that would change physical
    // placement or partitioning is refused loudly instead of being ignored.
    if create.like.is_some() || create.clone.is_some() {
        return Err(ParserError::NotSupported("CREATE TABLE LIKE/CLONE".into()));
    }
    if create.external
        || create.dynamic
        || create.iceberg
        || create.transient
        || create.volatile
        || create.strict
        || create.file_format.is_some()
        || create.location.is_some()
        || create.partition_by.is_some()
        || create.cluster_by.is_some()
        || create.clustered_by.is_some()
        || create.inherits.is_some()
        || create.partition_of.is_some()
        || create.for_values.is_some()
        || create.primary_key.is_some()
        || create.order_by.is_some()
        || create.on_cluster.is_some()
        || create.table_options != sa::CreateTableOptions::None
        || create.hive_distribution != sa::HiveDistributionStyle::NONE
    {
        // Partitioning, inheritance and physical-placement clauses change where
        // and how rows live; silently ignoring them would be a data-integrity
        // trap, so they are rejected until the storage layer supports them.
        return Err(ParserError::NotSupported("CREATE TABLE options".into()));
    }
    let mut columns = Vec::with_capacity(create.columns.len());
    let mut constraints = Vec::new();
    let lowered_name = object_name(&create.name)?;
    let table_name = lowered_name.last().cloned().unwrap_or_default();
    for column in &create.columns {
        let (def, mut extra) = lower_column(column, &table_name)?;
        // A column-level `REFERENCES t (b)` is a one-column table constraint.
        constraints.append(&mut extra);
        columns.push(def);
    }
    for constraint in &create.constraints {
        constraints.push(lower_table_constraint(constraint)?);
    }
    Ok(CreateTable {
        name: lowered_name,
        if_not_exists: create.if_not_exists,
        temporary: create.temporary,
        columns,
        constraints,
        query: match &create.query {
            Some(q) => Some(Box::new(lower_query(q)?)),
            None => None,
        },
    })
}

/// Lowers a column definition, returning any table constraints a column-level
/// clause turned into (`REFERENCES t (b)` on a column is a one-column
/// `FOREIGN KEY` table constraint).
///
/// `table_name` names the owning table, which a serial column needs to expand
/// to its `DEFAULT nextval('{table}_{column}_seq')`.
fn lower_column(
    column: &sa::ColumnDef,
    table_name: &str,
) -> Result<(ColumnDef, Vec<TableConstraint>)> {
    let mut def = ColumnDef {
        name: ident(&column.name),
        data_type: lower_data_type(&column.data_type)?,
        nullable: true,
        default: None,
        primary_key: false,
        unique: false,
        checks: Vec::new(),
    };
    // A serial pseudo-type is an integer column with an implicit `NOT NULL`
    // and a `nextval` default over an owned sequence, exactly as PostgreSQL
    // expands it. The sequence itself is created by the executor when the
    // table is.
    if let Some(plain) = serial_type(&column.data_type)? {
        def.data_type = plain;
        def.nullable = false;
        def.default = Some(Expr::Function(FunctionCall {
            name: "nextval".to_string(),
            args: vec![Expr::Literal(Value::Text(format!(
                "{}_{}_seq",
                table_name, def.name
            )))],
            distinct: false,
            star: false,
            filter: None,
            order_by: Vec::new(),
        }));
    };
    let mut constraints = Vec::new();
    for option in &column.options {
        // A `CONSTRAINT <name>` prefix names whichever option follows it.
        let declared_name = option.name.as_ref().map(ident);
        match &option.option {
            sa::ColumnOption::Null => def.nullable = true,
            sa::ColumnOption::NotNull => def.nullable = false,
            sa::ColumnOption::Default(expr) => def.default = Some(lower_expr(expr)?),
            sa::ColumnOption::PrimaryKey(_) => {
                def.primary_key = true;
                def.nullable = false;
            }
            sa::ColumnOption::Unique(_) => def.unique = true,
            sa::ColumnOption::Check(check) => def.checks.push(CheckConstraint {
                name: declared_name.or_else(|| check.name.as_ref().map(ident)),
                expr: lower_expr(&check.expr)?,
            }),
            sa::ColumnOption::Comment(_) | sa::ColumnOption::Collation(_) => {}
            sa::ColumnOption::ForeignKey(fk) => {
                // The engine stores the reference as metadata with NO ACTION
                // semantics; a declared action would silently behave
                // differently from PostgreSQL, so it is rejected instead.
                if fk.on_delete.is_some() || fk.on_update.is_some() {
                    return Err(ParserError::NotSupported(
                        "referential actions on REFERENCES".into(),
                    ));
                }
                constraints.push(TableConstraint::ForeignKey {
                    name: declared_name.or_else(|| fk.name.as_ref().map(ident)),
                    columns: vec![def.name.clone()],
                    ref_table: object_name(&fk.foreign_table)?,
                    ref_columns: fk.referred_columns.iter().map(ident).collect(),
                });
            }
            other => {
                return Err(ParserError::NotSupported(format!(
                    "column option `{other}`"
                )))
            }
        }
    }
    Ok((def, constraints))
}

/// The integer type a serial pseudo-type expands to; `None` when `ty` is not
/// one of them.
fn serial_type(ty: &sa::DataType) -> Result<Option<DataType>> {
    let sa::DataType::Custom(name, _) = ty else {
        return Ok(None);
    };
    let parts = object_name(name)?;
    let last = parts.last().map(String::as_str).unwrap_or_default();
    Ok(match last.to_lowercase().as_str() {
        "serial" | "serial4" => Some(DataType::Int4),
        "bigserial" | "serial8" => Some(DataType::Int8),
        "smallserial" | "serial2" => Some(DataType::Int2),
        _ => None,
    })
}

fn lower_table_constraint(constraint: &sa::TableConstraint) -> Result<TableConstraint> {
    let lowered = match constraint {
        sa::TableConstraint::PrimaryKey(pk) => {
            TableConstraint::PrimaryKey(index_columns(&pk.columns)?)
        }
        sa::TableConstraint::Unique(unique) => {
            TableConstraint::Unique(index_columns(&unique.columns)?)
        }
        sa::TableConstraint::Check(check) => TableConstraint::Check(CheckConstraint {
            name: check.name.as_ref().map(ident),
            expr: lower_expr(&check.expr)?,
        }),
        sa::TableConstraint::ForeignKey(fk) => TableConstraint::ForeignKey {
            name: fk.name.as_ref().map(ident),
            columns: fk.columns.iter().map(ident).collect(),
            ref_table: object_name(&fk.foreign_table)?,
            ref_columns: fk.referred_columns.iter().map(ident).collect(),
        },
        other => {
            return Err(ParserError::NotSupported(format!(
                "table constraint `{other}`"
            )))
        }
    };
    Ok(lowered)
}

fn index_columns(columns: &[sa::IndexColumn]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(columns.len());
    for column in columns {
        match &column.column.expr {
            sa::Expr::Identifier(id) => out.push(ident(id)),
            other => {
                return Err(ParserError::NotSupported(format!(
                    "expression index on `{other}`"
                )))
            }
        }
    }
    Ok(out)
}

/// Lowers `ALTER TABLE`.
///
/// One PostgreSQL operation can name several columns (`DROP COLUMN a, b`), so
/// every source operation lowers to a list of AST operations.
fn lower_alter_table(alter: &sa::AlterTable) -> Result<AlterTable> {
    if alter.if_exists {
        return Err(ParserError::NotSupported("ALTER TABLE IF EXISTS".into()));
    }
    if alter.on_cluster.is_some() || alter.location.is_some() || alter.table_type.is_some() {
        return Err(ParserError::NotSupported("ALTER TABLE options".into()));
    }
    // `ONLY` restricts an operation to the named table rather than to its
    // inheritance children; this engine has no inheritance, so both forms mean
    // the same thing and neither is dropped.
    let name = object_name(&alter.name)?;
    let table_name = name.last().cloned().unwrap_or_default();
    let mut operations = Vec::with_capacity(alter.operations.len());
    for operation in &alter.operations {
        operations.extend(lower_alter_table_operation(operation, &table_name)?);
    }
    Ok(AlterTable { name, operations })
}

fn lower_alter_table_operation(
    operation: &sa::AlterTableOperation,
    table_name: &str,
) -> Result<Vec<AlterTableOperation>> {
    let lowered = match operation {
        sa::AlterTableOperation::AddColumn {
            if_not_exists,
            column_def,
            column_position,
            ..
        } => {
            if column_position.is_some() {
                return Err(ParserError::NotSupported(
                    "column position (`FIRST`/`AFTER`)".into(),
                ));
            }
            let (column, extra) = lower_column(column_def, table_name)?;
            if !extra.is_empty() {
                // `ALTER TABLE ... ADD COLUMN x INT REFERENCES t (b)` would
                // have to attach a table constraint mid-statement.
                return Err(ParserError::NotSupported(
                    "column-level REFERENCES in ADD COLUMN".into(),
                ));
            }
            vec![AlterTableOperation::AddColumn {
                if_not_exists: *if_not_exists,
                column,
            }]
        }
        sa::AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            drop_behavior,
            ..
        } => {
            if drop_behavior.is_some() {
                return Err(ParserError::NotSupported(
                    "`CASCADE`/`RESTRICT` on DROP COLUMN".into(),
                ));
            }
            column_names
                .iter()
                .map(|name| AlterTableOperation::DropColumn {
                    if_exists: *if_exists,
                    name: ident(name),
                })
                .collect()
        }
        sa::AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => vec![AlterTableOperation::RenameColumn {
            name: ident(old_column_name),
            new_name: ident(new_column_name),
        }],
        sa::AlterTableOperation::RenameTable { table_name } => {
            let name = match table_name {
                sa::RenameTableNameKind::As(name) | sa::RenameTableNameKind::To(name) => name,
            };
            vec![AlterTableOperation::RenameTable(object_name(name)?)]
        }
        sa::AlterTableOperation::AlterColumn { column_name, op } => {
            let name = ident(column_name);
            vec![match op {
                sa::AlterColumnOperation::SetNotNull => AlterTableOperation::SetNotNull {
                    name,
                    not_null: true,
                },
                sa::AlterColumnOperation::DropNotNull => AlterTableOperation::SetNotNull {
                    name,
                    not_null: false,
                },
                sa::AlterColumnOperation::SetDefault { value } => AlterTableOperation::SetDefault {
                    name,
                    default: lower_expr(value)?,
                },
                sa::AlterColumnOperation::DropDefault => AlterTableOperation::DropDefault { name },
                sa::AlterColumnOperation::SetDataType {
                    data_type, using, ..
                } => {
                    // `USING` is an arbitrary conversion expression, which the
                    // engine cannot evaluate over the stored rows; refusing it is
                    // better than ignoring the conversion the user asked for.
                    if using.is_some() {
                        return Err(ParserError::NotSupported(
                            "`USING` on ALTER COLUMN TYPE".into(),
                        ));
                    }
                    AlterTableOperation::AlterColumnType {
                        name,
                        data_type: lower_data_type(data_type)?,
                    }
                }
                other => {
                    return Err(ParserError::NotSupported(format!(
                        "ALTER COLUMN `{}`",
                        first_words(&other.to_string())
                    )))
                }
            }]
        }
        sa::AlterTableOperation::AddConstraint {
            constraint,
            not_valid,
        } => {
            if *not_valid {
                return Err(ParserError::NotSupported("`NOT VALID` constraints".into()));
            }
            vec![AlterTableOperation::AddConstraint {
                name: table_constraint_name(constraint),
                constraint: lower_table_constraint(constraint)?,
            }]
        }
        sa::AlterTableOperation::DropConstraint {
            if_exists,
            name,
            drop_behavior,
        } => {
            if drop_behavior.is_some() {
                return Err(ParserError::NotSupported(
                    "`CASCADE`/`RESTRICT` on DROP CONSTRAINT".into(),
                ));
            }
            vec![AlterTableOperation::DropConstraint {
                if_exists: *if_exists,
                name: ident(name),
            }]
        }
        sa::AlterTableOperation::OwnerTo { new_owner } => match new_owner {
            sa::Owner::Ident(name) => vec![AlterTableOperation::OwnerTo(ident(name))],
            other => {
                return Err(ParserError::NotSupported(format!(
                    "ALTER TABLE ... OWNER TO {other}"
                )))
            }
        },
        other => {
            return Err(ParserError::NotSupported(format!(
                "ALTER TABLE action `{}`",
                first_words(&other.to_string())
            )))
        }
    };
    Ok(lowered)
}

/// The name an `ADD CONSTRAINT` gives a constraint, when the statement names one.
fn table_constraint_name(constraint: &sa::TableConstraint) -> Option<String> {
    let name = match constraint {
        sa::TableConstraint::PrimaryKey(primary) => primary.name.as_ref(),
        sa::TableConstraint::Unique(unique) => unique.name.as_ref(),
        _ => None,
    };
    name.map(ident)
}

fn lower_create_index(create: &sa::CreateIndex) -> Result<CreateIndex> {
    if create.predicate.is_some() {
        return Err(ParserError::NotSupported(
            "partial indexes (`WHERE`)".into(),
        ));
    }
    if !create.include.is_empty() {
        return Err(ParserError::NotSupported(
            "covering indexes (`INCLUDE`)".into(),
        ));
    }
    let using = match &create.using {
        None => IndexMethod::BTree,
        Some(sa::IndexType::BTree) => IndexMethod::BTree,
        Some(sa::IndexType::Hash) => IndexMethod::Hash,
        Some(other) => return Err(ParserError::NotSupported(format!("index method `{other}`"))),
    };
    let mut columns = Vec::with_capacity(create.columns.len());
    for column in &create.columns {
        let (name, asc) = match &column.column.expr {
            sa::Expr::Identifier(id) => (
                ident(id),
                !matches!(column.column.options.sort, Some(sa::OrderBySort::Desc)),
            ),
            other => {
                return Err(ParserError::NotSupported(format!(
                    "expression index on `{other}`"
                )))
            }
        };
        columns.push(IndexColumn { name, asc });
    }
    Ok(CreateIndex {
        name: match &create.name {
            Some(name) => Some(column_name(name)?),
            None => None,
        },
        table: object_name(&create.table_name)?,
        columns,
        unique: create.unique,
        if_not_exists: create.if_not_exists,
        using,
        predicate: None,
    })
}
