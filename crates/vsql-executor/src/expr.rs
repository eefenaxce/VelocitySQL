//! Bound (physical) scalar expressions.
//!
//! The planner rewrites the parser's *syntactic* expressions into this tree, in
//! which every column reference has already been resolved to a slot index. That
//! split is deliberate:
//!
//! * Name resolution, ambiguity detection, literal typing and aggregate
//!   extraction all happen once, at plan time, against a known schema.
//! * Evaluation is then a tight recursive walk over indices with no string
//!   lookups, which is what keeps the per-row cost low.
//!
//! `Column(i)` reads slot `i` of the *current* row; `Outer` reaches into an
//! enclosing row, which is how correlated subqueries see the outer query.

use std::sync::Arc;

use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use vsql_parser::{BinaryOp, UnaryOp};
use vsql_types::{DataType, Row, TypeError, Value};

use crate::error::ExecError;
use crate::plan::Plan;

/// A plan referenced from inside an expression.
///
/// Wrapped so that [`ScalarExpr`] can derive `PartialEq`: two subplans are the
/// same subplan when they *are* the same plan (pointer equality), not when they
/// happen to be structurally identical.
#[derive(Clone, Debug)]
pub struct SubPlan(pub Arc<Plan>);

impl PartialEq for SubPlan {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SubPlan {}

/// Enclosing rows for correlated evaluation, innermost first.
///
/// The environment is small (one row per enclosing query block) and is cloned
/// only when a subquery actually evaluates, which in practice is rare enough
/// that a full planner-level de-correlation pass is not worth it in M1.
#[derive(Clone, Debug, Default)]
pub struct EvalEnv {
    /// Enclosing rows, index 0 being the immediately enclosing block.
    pub outer: Vec<Row>,
    /// Statement start time.
    ///
    /// `now()` must not change halfway through a statement, which is exactly
    /// PostgreSQL's `transaction_timestamp()` behaviour.
    pub statement_time: Option<chrono::DateTime<chrono::Utc>>,
    /// Name of the connected database, reported by `current_database()`.
    pub database: Option<String>,
    /// Role the session acts as, reported by `current_user`.
    pub user: Option<String>,
    /// Bind parameters supplied via the extended query protocol, indexed by
    /// their 0-based position (`$1` resolves to `parameters[0]`).
    pub parameters: Vec<Value>,
    /// Live metadata for the virtual `pg_catalog` relations.
    pub catalog: Option<std::sync::Arc<crate::catalog_scan::CatalogSnapshot>>,
}

impl EvalEnv {
    /// Returns a new environment with `row` pushed as the innermost enclosing row.
    pub fn push(&self, row: &Row) -> EvalEnv {
        let mut outer = Vec::with_capacity(self.outer.len() + 1);
        outer.push(row.clone());
        outer.extend_from_slice(&self.outer);
        EvalEnv {
            outer,
            statement_time: self.statement_time,
            database: self.database.clone(),
            user: self.user.clone(),
            parameters: self.parameters.clone(),
            catalog: self.catalog.clone(),
        }
    }
}

/// A bound scalar expression.
#[derive(Clone, Debug, PartialEq)]
pub enum ScalarExpr {
    /// A constant.
    Const(Value),
    /// Slot `n` of the current row.
    Column(usize),
    /// Result of an aggregate, materialised at slot `n` of the current row.
    ///
    /// The planner extracts aggregate calls, plans them as an `Aggregate` node
    /// and replaces each call with this marker. Because an aggregate's argument
    /// is bound in "no aggregates allowed" mode, nesting is impossible by
    /// construction rather than by a runtime check.
    Aggregate(usize),
    /// Slot `index` of an enclosing row, `levels_up` blocks away.
    Outer {
        /// 1 for the immediately enclosing block.
        levels_up: usize,
        /// Slot inside that row.
        index: usize,
    },
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<ScalarExpr>,
    },
    /// A binary operation.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<ScalarExpr>,
        /// Right operand.
        right: Box<ScalarExpr>,
    },
    /// An explicit cast.
    Cast {
        /// Source expression.
        expr: Box<ScalarExpr>,
        /// Target type.
        data_type: DataType,
        /// `TRY_CAST`: converts failures into `NULL` instead of errors.
        try_cast: bool,
    },
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// The tested expression.
        expr: Box<ScalarExpr>,
        /// `true` for `IS NOT NULL`.
        negated: bool,
    },
    /// `expr IS [NOT] {TRUE|FALSE|UNKNOWN}`.
    IsBool {
        /// The tested expression.
        expr: Box<ScalarExpr>,
        /// `Some(true)`/`Some(false)` for `IS TRUE`/`IS FALSE`, `None` for `IS UNKNOWN`.
        value: Option<bool>,
        /// `true` for the `IS NOT ...` form.
        negated: bool,
    },
    /// `a IS [NOT] DISTINCT FROM b`, which never returns `NULL`.
    IsDistinctFrom {
        /// Left operand.
        left: Box<ScalarExpr>,
        /// Right operand.
        right: Box<ScalarExpr>,
        /// `true` for `IS NOT DISTINCT FROM`.
        negated: bool,
    },
    /// `expr [NOT] IN (list)`.
    InList {
        /// Tested expression.
        expr: Box<ScalarExpr>,
        /// Candidates.
        list: Vec<ScalarExpr>,
        /// `true` for `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] LIKE pattern`.
    Like {
        /// Tested expression.
        expr: Box<ScalarExpr>,
        /// Pattern.
        pattern: Box<ScalarExpr>,
        /// `true` for `NOT LIKE`.
        negated: bool,
        /// `true` for `ILIKE`.
        case_insensitive: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// Tested expression.
        expr: Box<ScalarExpr>,
        /// Lower bound.
        low: Box<ScalarExpr>,
        /// Upper bound.
        high: Box<ScalarExpr>,
        /// `true` for `NOT BETWEEN`.
        negated: bool,
    },
    /// A `CASE` expression.
    Case {
        /// Operand of the simple form.
        operand: Option<Box<ScalarExpr>>,
        /// `WHEN .. THEN ..` pairs.
        whens: Vec<(ScalarExpr, ScalarExpr)>,
        /// `ELSE` result.
        else_result: Option<Box<ScalarExpr>>,
    },
    /// A scalar function call.
    Function {
        /// Lower-case function name.
        name: String,
        /// Bound arguments.
        args: Vec<ScalarExpr>,
    },
    /// `ARRAY[...]`.
    Array(Vec<ScalarExpr>),
    /// A row constructor.
    Row(Vec<ScalarExpr>),
    /// A scalar subquery.
    ScalarSubquery(SubPlan),
    /// `[NOT] EXISTS (subquery)`.
    Exists {
        /// The subquery.
        plan: SubPlan,
        /// `true` for `NOT EXISTS`.
        negated: bool,
    },
    /// `expr [NOT] IN (subquery)`.
    InSubquery {
        /// Tested expression.
        expr: Box<ScalarExpr>,
        /// The subquery, expected to return one column.
        plan: SubPlan,
        /// `true` for `NOT IN`.
        negated: bool,
    },
    /// `left op ANY/ALL (values)`.
    ///
    /// PostgreSQL rewrites `= ANY (subquery)` into `IN (subquery)` and
    /// `<> ALL (subquery)` into `NOT IN (subquery)`, which is what the planner
    /// does too; this node stays for the comparisons that have to look at each
    /// candidate, including `= ANY (current_schemas(false))`.
    AnyAll {
        /// Left operand.
        left: Box<ScalarExpr>,
        /// The comparison applied to each candidate.
        op: BinaryOp,
        /// Where the candidates come from.
        source: AnyAllSource,
        /// `true` for `ALL`, `false` for `ANY`.
        is_all: bool,
    },
    /// `expr[index]`: a one-based array element access.
    Subscript {
        /// The indexed expression.
        expr: Box<ScalarExpr>,
        /// The one-based index.
        index: Box<ScalarExpr>,
    },
    /// A bind parameter, e.g. `$1`; resolved from the session at evaluation time.
    Parameter(usize),
}

/// Where an `ANY`/`ALL` comparison takes its candidates from.
#[derive(Clone, Debug, PartialEq)]
pub enum AnyAllSource {
    /// An array-valued expression, e.g. `schema = ANY (current_schemas(false))`.
    Array(Box<ScalarExpr>),
    /// The first column of a subquery, e.g. `oid = ANY (SELECT relid FROM t)`.
    Subquery(SubPlan),
}

impl ScalarExpr {
    /// A constant expression.
    pub fn constant(value: Value) -> ScalarExpr {
        ScalarExpr::Const(value)
    }

    /// Evaluates the expression against `row` and the enclosing rows in `env`.
    pub fn eval(&self, row: &Row, env: &EvalEnv) -> Result<Value, ExecError> {
        let value = match self {
            ScalarExpr::Const(value) => value.clone(),
            ScalarExpr::Parameter(index) => env
                .parameters
                .get(*index)
                .cloned()
                .ok_or(ExecError::UndefinedParameter(*index + 1))?,
            ScalarExpr::Column(index) | ScalarExpr::Aggregate(index) => row
                .get(*index)
                .cloned()
                .ok_or_else(|| ExecError::other(format!("column slot {index} is out of range")))?,
            ScalarExpr::Outer { levels_up, index } => {
                let outer = env.outer.get(levels_up - 1).ok_or_else(|| {
                    ExecError::other("correlated reference outside the query tree")
                })?;
                outer.get(*index).cloned().ok_or_else(|| {
                    ExecError::other(format!("outer column slot {index} is out of range"))
                })?
            }
            ScalarExpr::Unary { op, expr } => {
                let value = expr.eval(row, env)?;
                eval_unary(*op, value)?
            }
            ScalarExpr::Binary { op, left, right } => {
                let left = left.eval(row, env)?;
                let right = right.eval(row, env)?;
                eval_binary(*op, &left, &right)?
            }
            ScalarExpr::Cast {
                expr,
                data_type,
                try_cast,
            } => {
                let value = expr.eval(row, env)?;
                match value.cast_to(data_type) {
                    Ok(value) => value,
                    Err(_) if *try_cast => Value::Null,
                    Err(error) => return Err(error.into()),
                }
            }
            ScalarExpr::IsNull { expr, negated } => {
                let is_null = expr.eval(row, env)?.is_null();
                Value::Bool(is_null != *negated)
            }
            ScalarExpr::IsBool {
                expr,
                value,
                negated,
            } => {
                let evaluated = expr.eval(row, env)?;
                let actual = match evaluated {
                    Value::Null => None,
                    other => other.as_bool(),
                };
                let matches = actual == *value;
                Value::Bool(matches != *negated)
            }
            ScalarExpr::IsDistinctFrom {
                left,
                right,
                negated,
            } => {
                let left = left.eval(row, env)?;
                let right = right.eval(row, env)?;
                let distinct = match (left.is_null(), right.is_null()) {
                    (true, true) => false,
                    (true, false) | (false, true) => true,
                    (false, false) => {
                        !matches!(left.compare(&right, "=")?, Some(std::cmp::Ordering::Equal))
                    }
                };
                Value::Bool(distinct != *negated)
            }
            ScalarExpr::InList {
                expr,
                list,
                negated,
            } => {
                let value = expr.eval(row, env)?;
                let mut found = false;
                let mut has_null = false;
                for candidate in list {
                    let candidate = candidate.eval(row, env)?;
                    match value.compare(&candidate, "=")? {
                        None => has_null = true,
                        Some(std::cmp::Ordering::Equal) => {
                            found = true;
                            break;
                        }
                        Some(_) => {}
                    }
                }
                if found {
                    Value::Bool(!*negated)
                } else if has_null {
                    Value::Null
                } else {
                    Value::Bool(*negated)
                }
            }
            ScalarExpr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let value = expr.eval(row, env)?;
                let pattern = pattern.eval(row, env)?;
                match (value.as_str(), pattern.as_str()) {
                    (Some(value), Some(pattern)) => {
                        let matched = like_match(value, pattern, *case_insensitive);
                        Value::Bool(matched != *negated)
                    }
                    _ => Value::Null,
                }
            }
            ScalarExpr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let value = expr.eval(row, env)?;
                let low = low.eval(row, env)?;
                let high = high.eval(row, env)?;
                // `BETWEEN` is defined as `x >= low AND x <= high`.
                let above = match value.compare(&low, ">=")? {
                    None => return Ok(Value::Null),
                    Some(ordering) => ordering != std::cmp::Ordering::Less,
                };
                if !above {
                    return Ok(Value::Bool(*negated));
                }
                match value.compare(&high, "<=")? {
                    None => Value::Null,
                    Some(ordering) => {
                        Value::Bool((ordering != std::cmp::Ordering::Greater) != *negated)
                    }
                }
            }
            ScalarExpr::Case {
                operand,
                whens,
                else_result,
            } => {
                let operand = match operand {
                    Some(operand) => Some(operand.eval(row, env)?),
                    None => None,
                };
                let mut result = None;
                for (condition, value) in whens {
                    let matches = match &operand {
                        Some(operand) => {
                            let candidate = condition.eval(row, env)?;
                            matches!(
                                operand.compare(&candidate, "=")?,
                                Some(std::cmp::Ordering::Equal)
                            )
                        }
                        None => matches!(condition.eval(row, env)?.as_bool(), Some(true)),
                    };
                    if matches {
                        result = Some(value.eval(row, env)?);
                        break;
                    }
                }
                match result {
                    Some(value) => value,
                    None => match else_result {
                        Some(else_result) => else_result.eval(row, env)?,
                        None => Value::Null,
                    },
                }
            }
            ScalarExpr::Function { name, args } => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(arg.eval(row, env)?);
                }
                crate::functions::call_scalar(name, &values, env)?
            }
            ScalarExpr::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(item.eval(row, env)?);
                }
                Value::Array(values)
            }
            ScalarExpr::Row(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(item.eval(row, env)?);
                }
                Value::Array(values)
            }
            ScalarExpr::Subscript { expr, index } => {
                let operand = expr.eval(row, env)?;
                let index = index.eval(row, env)?;
                eval_subscript(&operand, &index)?
            }
            ScalarExpr::ScalarSubquery(plan) => {
                let rows = crate::exec::run_plan(&plan.0, &env.push(row))?;
                match rows.len() {
                    0 => Value::Null,
                    1 => rows
                        .into_iter()
                        .next()
                        .expect("checked length")
                        .into_iter()
                        .next()
                        .unwrap_or(Value::Null),
                    _ => return Err(ExecError::SubqueryTooManyRows),
                }
            }
            ScalarExpr::Exists { plan, negated } => {
                let rows = crate::exec::run_plan(&plan.0, &env.push(row))?;
                Value::Bool(!rows.is_empty() != *negated)
            }
            ScalarExpr::InSubquery {
                expr,
                plan,
                negated,
            } => {
                let value = expr.eval(row, env)?;
                let rows = crate::exec::run_plan(&plan.0, &env.push(row))?;
                let mut found = false;
                let mut has_null = false;
                for candidate in rows {
                    let candidate = candidate.into_iter().next().unwrap_or(Value::Null);
                    match value.compare(&candidate, "=")? {
                        None => has_null = true,
                        Some(std::cmp::Ordering::Equal) => {
                            found = true;
                            break;
                        }
                        Some(_) => {}
                    }
                }
                if found {
                    Value::Bool(!*negated)
                } else if has_null {
                    Value::Null
                } else {
                    Value::Bool(*negated)
                }
            }
            ScalarExpr::AnyAll {
                left,
                op,
                source,
                is_all,
            } => {
                let value = left.eval(row, env)?;
                let candidates = match source {
                    AnyAllSource::Array(array) => match array.eval(row, env)? {
                        Value::Array(items) => items,
                        // PostgreSQL coerces an untyped literal inside `= ANY(...)`
                        // to the left operand's array type, so `int = ANY('{1,2}')`
                        // reads the text as an array of integers. A string that is
                        // not shaped like an array stays a single candidate, which
                        // keeps `= ANY(current_schemas(false))`-style sources working.
                        Value::Text(text) => match Value::parse_array_literal(&text) {
                            Some(items) => items,
                            None => vec![Value::Text(text)],
                        },
                        other => vec![other],
                    },
                    AnyAllSource::Subquery(plan) => crate::exec::run_plan(&plan.0, &env.push(row))?
                        .into_iter()
                        .map(|row| row.into_iter().next().unwrap_or(Value::Null))
                        .collect(),
                };
                // An `ANY` that matched nothing is false and an `ALL` that found
                // no counterexample is true; a `NULL` candidate makes the answer
                // unknown unless a candidate has already decided it.
                let mut unknown = false;
                for candidate in &candidates {
                    match compare_candidate(&value, *op, candidate)? {
                        Some(true) if !*is_all => return Ok(Value::Bool(true)),
                        Some(false) if *is_all => return Ok(Value::Bool(false)),
                        Some(_) => {}
                        None => unknown = true,
                    }
                }
                if unknown {
                    Value::Null
                } else {
                    Value::Bool(*is_all)
                }
            }
        };
        Ok(value)
    }

    /// Whether the expression tree contains no column or subquery references.
    pub fn is_constant(&self) -> bool {
        match self {
            ScalarExpr::Const(_) => true,
            ScalarExpr::Column(_)
            | ScalarExpr::Aggregate(_)
            | ScalarExpr::Outer { .. }
            | ScalarExpr::Parameter(_) => false,
            ScalarExpr::ScalarSubquery(_)
            | ScalarExpr::Exists { .. }
            | ScalarExpr::InSubquery { .. } => false,
            ScalarExpr::AnyAll { left, source, .. } => match source {
                AnyAllSource::Array(array) => left.is_constant() && array.is_constant(),
                AnyAllSource::Subquery(_) => false,
            },
            ScalarExpr::Unary { expr, .. } | ScalarExpr::IsNull { expr, .. } => expr.is_constant(),
            ScalarExpr::IsBool { expr, .. } => expr.is_constant(),
            ScalarExpr::Cast { expr, .. } => expr.is_constant(),
            ScalarExpr::IsDistinctFrom { left, right, .. }
            | ScalarExpr::Binary { left, right, .. } => left.is_constant() && right.is_constant(),
            ScalarExpr::InList { expr, list, .. } => {
                expr.is_constant() && list.iter().all(ScalarExpr::is_constant)
            }
            ScalarExpr::Like { expr, pattern, .. } => expr.is_constant() && pattern.is_constant(),
            ScalarExpr::Between {
                expr, low, high, ..
            } => expr.is_constant() && low.is_constant() && high.is_constant(),
            ScalarExpr::Case {
                operand,
                whens,
                else_result,
            } => {
                operand.as_ref().map(|o| o.is_constant()).unwrap_or(true)
                    && whens
                        .iter()
                        .all(|(c, v)| c.is_constant() && v.is_constant())
                    && else_result
                        .as_ref()
                        .map(|e| e.is_constant())
                        .unwrap_or(true)
            }
            ScalarExpr::Function { args, .. } | ScalarExpr::Array(args) | ScalarExpr::Row(args) => {
                args.iter().all(ScalarExpr::is_constant)
            }
            ScalarExpr::Subscript { expr, index } => expr.is_constant() && index.is_constant(),
        }
    }

    /// Evaluates a constant expression, returning `None` when it needs a row.
    pub fn eval_constant(&self) -> Result<Option<Value>, ExecError> {
        if !self.is_constant() {
            return Ok(None);
        }
        Ok(Some(self.eval(&Row::new(), &EvalEnv::default())?))
    }

    /// Best-effort static type of the expression.
    ///
    /// Used by the planner for two things: deciding how to coerce an untyped
    /// string literal against its counterpart, and labelling result columns.
    /// Returning `None` is always safe — it only means "unknown at plan time".
    pub fn infer_type(&self, schema: &[crate::Field]) -> Option<DataType> {
        match self {
            ScalarExpr::Const(Value::Null) => None,
            ScalarExpr::Const(value) => Some(value.type_of()),
            ScalarExpr::Parameter(_) => None,
            ScalarExpr::Column(index) => schema.get(*index).map(|field| field.data_type.clone()),
            ScalarExpr::Aggregate(_) => None,
            ScalarExpr::Outer { .. } => None,
            ScalarExpr::Cast { data_type, .. } => Some(data_type.clone()),
            ScalarExpr::Unary { op, expr } => match op {
                UnaryOp::Not => Some(DataType::Bool),
                _ => expr.infer_type(schema),
            },
            ScalarExpr::Binary { op, left, right } => match op {
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
                | BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::IsDistinctFrom
                | BinaryOp::IsNotDistinctFrom
                | BinaryOp::RegexMatch
                | BinaryOp::RegexIMatch
                | BinaryOp::RegexNotMatch
                | BinaryOp::RegexNotIMatch => Some(DataType::Bool),
                BinaryOp::Concat => Some(DataType::Text),
                _ => {
                    let left = left.infer_type(schema)?;
                    let right = right.infer_type(schema)?;
                    Some(numeric_result_type(&left, &right))
                }
            },
            ScalarExpr::IsNull { .. }
            | ScalarExpr::IsBool { .. }
            | ScalarExpr::IsDistinctFrom { .. }
            | ScalarExpr::InList { .. }
            | ScalarExpr::Like { .. }
            | ScalarExpr::Between { .. }
            | ScalarExpr::Exists { .. }
            | ScalarExpr::InSubquery { .. }
            | ScalarExpr::AnyAll { .. } => Some(DataType::Bool),
            ScalarExpr::Case {
                whens, else_result, ..
            } => whens
                .iter()
                .find_map(|(_, value)| value.infer_type(schema))
                .or_else(|| {
                    else_result
                        .as_ref()
                        .and_then(|value| value.infer_type(schema))
                }),
            ScalarExpr::Function { name, args } => {
                crate::functions::scalar_return_type(name, args, schema)
            }
            ScalarExpr::Array(items) => {
                let element = items
                    .iter()
                    .find_map(|item| item.infer_type(schema))
                    .unwrap_or(DataType::Text);
                Some(DataType::Array(Box::new(element)))
            }
            ScalarExpr::Row(_) => None,
            ScalarExpr::Subscript { expr, .. } => match expr.infer_type(schema) {
                // An array element keeps the element type; the text form of a
                // catalogue vector (`{1,2}`) yields its elements as text.
                Some(DataType::Array(element)) => Some(*element),
                Some(DataType::Text) => Some(DataType::Text),
                _ => None,
            },
            ScalarExpr::ScalarSubquery(plan) => {
                plan.0.schema().first().map(|field| field.data_type.clone())
            }
        }
    }

    /// Visits the immediate sub-expressions.
    ///
    /// `Aggregate` and `Outer` are leaves: the first is already computed, the
    /// second belongs to an enclosing query block, so neither can violate a
    /// `GROUP BY` rule.
    pub fn for_each_child(&self, f: &mut dyn FnMut(&ScalarExpr)) {
        match self {
            ScalarExpr::Const(_)
            | ScalarExpr::Column(_)
            | ScalarExpr::Aggregate(_)
            | ScalarExpr::Outer { .. }
            | ScalarExpr::ScalarSubquery(_)
            | ScalarExpr::Exists { .. }
            | ScalarExpr::Parameter(_) => {}
            ScalarExpr::Unary { expr, .. }
            | ScalarExpr::IsNull { expr, .. }
            | ScalarExpr::IsBool { expr, .. }
            | ScalarExpr::Cast { expr, .. } => f(expr),
            ScalarExpr::IsDistinctFrom { left, right, .. }
            | ScalarExpr::Binary { left, right, .. } => {
                f(left);
                f(right);
            }
            ScalarExpr::InList { expr, list, .. } => {
                f(expr);
                for item in list {
                    f(item);
                }
            }
            ScalarExpr::Like { expr, pattern, .. } => {
                f(expr);
                f(pattern);
            }
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                f(expr);
                f(low);
                f(high);
            }
            ScalarExpr::Case {
                operand,
                whens,
                else_result,
            } => {
                if let Some(operand) = operand {
                    f(operand);
                }
                for (condition, value) in whens {
                    f(condition);
                    f(value);
                }
                if let Some(else_result) = else_result {
                    f(else_result);
                }
            }
            ScalarExpr::Function { args, .. } | ScalarExpr::Array(args) | ScalarExpr::Row(args) => {
                for arg in args {
                    f(arg);
                }
            }
            ScalarExpr::Subscript { expr, index } => {
                f(expr);
                f(index);
            }
            ScalarExpr::InSubquery { expr, .. } => f(expr),
            ScalarExpr::AnyAll { left, source, .. } => {
                f(left);
                if let AnyAllSource::Array(array) = source {
                    f(array);
                }
            }
        }
    }

    /// Collects every `Column` reference in the tree.
    pub fn collect_columns(&self, out: &mut Vec<usize>) {
        match self {
            ScalarExpr::Column(index) => out.push(*index),
            ScalarExpr::Aggregate(_)
            | ScalarExpr::Const(_)
            | ScalarExpr::Outer { .. }
            | ScalarExpr::Parameter(_) => {}
            ScalarExpr::Unary { expr, .. }
            | ScalarExpr::IsNull { expr, .. }
            | ScalarExpr::IsBool { expr, .. }
            | ScalarExpr::Cast { expr, .. } => expr.collect_columns(out),
            ScalarExpr::IsDistinctFrom { left, right, .. }
            | ScalarExpr::Binary { left, right, .. } => {
                left.collect_columns(out);
                right.collect_columns(out);
            }
            ScalarExpr::InList { expr, list, .. } => {
                expr.collect_columns(out);
                for item in list {
                    item.collect_columns(out);
                }
            }
            ScalarExpr::Like { expr, pattern, .. } => {
                expr.collect_columns(out);
                pattern.collect_columns(out);
            }
            ScalarExpr::Between {
                expr, low, high, ..
            } => {
                expr.collect_columns(out);
                low.collect_columns(out);
                high.collect_columns(out);
            }
            ScalarExpr::Case {
                operand,
                whens,
                else_result,
            } => {
                if let Some(operand) = operand {
                    operand.collect_columns(out);
                }
                for (condition, value) in whens {
                    condition.collect_columns(out);
                    value.collect_columns(out);
                }
                if let Some(else_result) = else_result {
                    else_result.collect_columns(out);
                }
            }
            ScalarExpr::Subscript { expr, index } => {
                expr.collect_columns(out);
                index.collect_columns(out);
            }
            ScalarExpr::Function { args, .. } | ScalarExpr::Array(args) | ScalarExpr::Row(args) => {
                for arg in args {
                    arg.collect_columns(out);
                }
            }
            ScalarExpr::ScalarSubquery(_) | ScalarExpr::Exists { .. } => {}
            ScalarExpr::InSubquery { expr, .. } => expr.collect_columns(out),
            ScalarExpr::AnyAll { left, source, .. } => {
                left.collect_columns(out);
                if let AnyAllSource::Array(array) = source {
                    array.collect_columns(out);
                }
            }
        }
    }
}

/// The type an arithmetic operation between `left` and `right` produces.
pub fn numeric_result_type(left: &DataType, right: &DataType) -> DataType {
    use DataType::*;
    let rank = |ty: &DataType| match ty {
        Int2 => 0,
        Int4 => 1,
        Int8 => 2,
        Numeric { .. } => 3,
        Float4 => 4,
        Float8 => 5,
        _ => -1,
    };
    let (left_rank, right_rank) = (rank(left), rank(right));
    if left_rank < 0 || right_rank < 0 {
        return Float8;
    }
    match left_rank.max(right_rank) {
        // `int2 + int2` still yields `int2` in PostgreSQL, but the C `int`
        // promotion means the value fits; we widen to `int4` to rule out the
        // silent wrap-around that follows from a narrowed result slot.
        0 | 1 => Int4,
        2 => Int8,
        3 => Numeric {
            precision: None,
            scale: None,
        },
        4 => Float4,
        _ => Float8,
    }
}

fn eval_unary(op: UnaryOp, value: Value) -> Result<Value, ExecError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let result = match op {
        UnaryOp::Plus => value,
        UnaryOp::Not => match value.as_bool() {
            Some(value) => Value::Bool(!value),
            None => {
                return Err(ExecError::other(format!(
                    "argument of NOT must be type boolean, not type {}",
                    value.type_of().format_type()
                )))
            }
        },
        UnaryOp::Negate => match value {
            Value::Int2(v) => Value::Int2(-v),
            Value::Int4(v) => Value::Int4(-v),
            Value::Int8(v) => v
                .checked_neg()
                .map(Value::Int8)
                .ok_or_else(|| ExecError::other("bigint out of range"))?,
            Value::Float4(v) => Value::Float4(-v),
            Value::Float8(v) => Value::Float8(-v),
            Value::Numeric(v) => Value::Numeric(-v),
            other => {
                return Err(ExecError::other(format!(
                    "operator does not exist: - {}",
                    other.type_of().format_type()
                )))
            }
        },
        UnaryOp::BitNot => match value.as_i64() {
            Some(value) => Value::Int8(!value),
            None => {
                return Err(ExecError::other(format!(
                    "operator does not exist: ~ {}",
                    value.type_of().format_type()
                )))
            }
        },
    };
    Ok(result)
}

/// Evaluates a binary operation with PostgreSQL's three-valued logic.
pub fn eval_binary(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    use BinaryOp::*;
    match op {
        And => {
            // `NULL AND FALSE` is `FALSE`, `NULL AND TRUE` is `NULL`.
            return Ok(match (left.as_bool(), right.as_bool()) {
                (Some(false), _) | (_, Some(false)) => Value::Bool(false),
                (Some(true), Some(true)) => Value::Bool(true),
                _ if left.is_null() || right.is_null() => Value::Null,
                _ => return Err(ExecError::other("argument of AND must be type boolean")),
            });
        }
        Or => {
            return Ok(match (left.as_bool(), right.as_bool()) {
                (Some(true), _) | (_, Some(true)) => Value::Bool(true),
                (Some(false), Some(false)) => Value::Bool(false),
                _ if left.is_null() || right.is_null() => Value::Null,
                _ => return Err(ExecError::other("argument of OR must be type boolean")),
            });
        }
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            let symbol = match op {
                Eq => "=",
                NotEq => "<>",
                Lt => "<",
                LtEq => "<=",
                Gt => ">",
                GtEq => ">=",
                _ => unreachable!("matched above"),
            };
            return Ok(match left.compare(right, symbol)? {
                None => Value::Null,
                Some(ordering) => {
                    let holds = match op {
                        Eq => ordering == std::cmp::Ordering::Equal,
                        NotEq => ordering != std::cmp::Ordering::Equal,
                        Lt => ordering == std::cmp::Ordering::Less,
                        LtEq => ordering != std::cmp::Ordering::Greater,
                        Gt => ordering == std::cmp::Ordering::Greater,
                        GtEq => ordering != std::cmp::Ordering::Less,
                        _ => unreachable!("matched above"),
                    };
                    Value::Bool(holds)
                }
            });
        }
        Concat => {
            if left.is_null() || right.is_null() {
                return Ok(Value::Null);
            }
            return Ok(Value::Text(format!("{left}{right}")));
        }
        RegexMatch | RegexIMatch | RegexNotMatch | RegexNotIMatch => {
            return Err(ExecError::not_supported(
                "regular expression operators are planned for milestone M5",
            ));
        }
        _ => {}
    }
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    arithmetic(op, left, right)
}

fn arithmetic(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    use BinaryOp::*;
    // PostgreSQL reads an untyped literal or a driver-sent text parameter as
    // the other operand's numeric type: `1 + '2'` is `3`, and `1 + $1` with
    // `$1` bound to `'2'` behaves the same. A string that is not a number is
    // still an operator error, matching PostgreSQL's invalid-input report.
    let left_holder: Value;
    let right_holder: Value;
    let left: &Value = match (left, right) {
        (Value::Text(text), other) if other.is_numeric() => {
            left_holder = coerce_text_number(text, other)
                .ok_or_else(|| undefined_operator(op, left, right))?;
            &left_holder
        }
        (value, _) => value,
    };
    let right: &Value = match (right, left) {
        (Value::Text(text), other) if other.is_numeric() => {
            right_holder = coerce_text_number(text, other)
                .ok_or_else(|| undefined_operator(op, right, left))?;
            &right_holder
        }
        (value, _) => value,
    };
    if matches!(left, Value::Numeric(_)) || matches!(right, Value::Numeric(_)) {
        let (Some(a), Some(b)) = (left.as_decimal(), right.as_decimal()) else {
            return Err(undefined_operator(op, left, right));
        };
        let result = match op {
            Plus => a.checked_add(b),
            Minus => a.checked_sub(b),
            Multiply => a.checked_mul(b),
            Divide => {
                if b.is_zero() {
                    return Err(ExecError::DivisionByZero);
                }
                a.checked_div(b)
            }
            Modulo => {
                if b.is_zero() {
                    return Err(ExecError::DivisionByZero);
                }
                a.checked_rem(b)
            }
            Pow => return float_arithmetic(op, left, right),
            _ => return Err(undefined_operator(op, left, right)),
        };
        return match result {
            Some(value) => Ok(Value::Numeric(value)),
            None => Err(ExecError::other("numeric out of range")),
        };
    }
    if matches!(left, Value::Float4(_) | Value::Float8(_))
        || matches!(right, Value::Float4(_) | Value::Float8(_))
    {
        return float_arithmetic(op, left, right);
    }
    if left.is_numeric() && right.is_numeric() {
        return int_arithmetic(op, left, right);
    }
    Err(undefined_operator(op, left, right))
}

/// Reads `text` as a number of the same family as `target`, mirroring how
/// PostgreSQL coerces an unknown-typed operand of an arithmetic operator.
///
/// Returns `None` when the string is not a number or the target is not one of
/// the numeric families, leaving the caller to report the missing operator.
fn coerce_text_number(text: &str, target: &Value) -> Option<Value> {
    let text = text.trim();
    if matches!(target, Value::Int2(_) | Value::Int4(_) | Value::Int8(_)) {
        let parsed: i64 = text.parse().ok()?;
        return Some(if i32::MIN as i64 <= parsed && parsed <= i32::MAX as i64 {
            Value::Int4(parsed as i32)
        } else {
            Value::Int8(parsed)
        });
    }
    if matches!(target, Value::Float4(_) | Value::Float8(_)) {
        let parsed: f64 = text.parse().ok()?;
        return Some(Value::Float8(parsed));
    }
    if matches!(target, Value::Numeric(_)) {
        let parsed: Decimal = text.parse().ok()?;
        return Some(Value::Numeric(parsed));
    }
    None
}

fn int_arithmetic(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    use BinaryOp::*;
    let (Some(a), Some(b)) = (left.as_i64(), right.as_i64()) else {
        return Err(undefined_operator(op, left, right));
    };
    let computed = match op {
        Plus => a.checked_add(b),
        Minus => a.checked_sub(b),
        Multiply => a.checked_mul(b),
        Divide => {
            if b == 0 {
                return Err(ExecError::DivisionByZero);
            }
            a.checked_div(b)
        }
        Modulo => {
            if b == 0 {
                return Err(ExecError::DivisionByZero);
            }
            a.checked_rem(b)
        }
        Pow => return float_arithmetic(op, left, right),
        BitAnd => Some(a & b),
        BitOr => Some(a | b),
        BitXor => Some(a ^ b),
        ShiftLeft => Some(a.wrapping_shl(b as u32)),
        ShiftRight => Some(a.wrapping_shr(b as u32)),
        _ => return Err(undefined_operator(op, left, right)),
    };
    let Some(computed) = computed else {
        return Err(ExecError::other("integer out of range"));
    };
    // The result widens to the wider of the two operands, floored at `int4`:
    // PostgreSQL's own arithmetic never narrows.
    let wider = matches!(left, Value::Int8(_)) || matches!(right, Value::Int8(_));
    Ok(if wider {
        Value::Int8(computed)
    } else {
        Value::Int4(computed as i32)
    })
}

fn float_arithmetic(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    use BinaryOp::*;
    let (Some(a), Some(b)) = (left.as_f64(), right.as_f64()) else {
        return Err(undefined_operator(op, left, right));
    };
    let computed = match op {
        Plus => a + b,
        Minus => a - b,
        Multiply => a * b,
        Divide => {
            if b == 0.0 {
                return Err(ExecError::DivisionByZero);
            }
            a / b
        }
        Modulo => {
            if b == 0.0 {
                return Err(ExecError::DivisionByZero);
            }
            a % b
        }
        Pow => a.powf(b),
        _ => return Err(undefined_operator(op, left, right)),
    };
    Ok(Value::Float8(computed))
}

/// Applies one comparison of an `ANY`/`ALL` expression to a candidate value.
///
/// `Ok(None)` is SQL's unknown, which a `NULL` on either side produces.
fn compare_candidate(
    left: &Value,
    op: BinaryOp,
    candidate: &Value,
) -> Result<Option<bool>, ExecError> {
    let Some(ordering) = left.compare(candidate, crate::render::op_symbol(op))? else {
        return Ok(None);
    };
    let satisfied = match op {
        BinaryOp::Eq => ordering == std::cmp::Ordering::Equal,
        BinaryOp::NotEq => ordering != std::cmp::Ordering::Equal,
        BinaryOp::Lt => ordering == std::cmp::Ordering::Less,
        BinaryOp::LtEq => ordering != std::cmp::Ordering::Greater,
        BinaryOp::Gt => ordering == std::cmp::Ordering::Greater,
        BinaryOp::GtEq => ordering != std::cmp::Ordering::Less,
        other => {
            return Err(ExecError::not_supported(format!(
                "{} ANY/ALL",
                crate::render::op_symbol(other)
            )))
        }
    };
    Ok(Some(satisfied))
}

fn undefined_operator(op: BinaryOp, left: &Value, right: &Value) -> ExecError {
    let symbol = match op {
        BinaryOp::Plus => "+",
        BinaryOp::Minus => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Modulo => "%",
        BinaryOp::Concat => "||",
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::GtEq => ">=",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::Pow => "^",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "#",
        BinaryOp::ShiftLeft => "<<",
        BinaryOp::ShiftRight => ">>",
        _ => "?",
    };
    let error: TypeError = TypeError::UndefinedComparison {
        left: left.type_of().format_type(),
        op: symbol.to_string(),
        right: right.type_of().format_type(),
    };
    ExecError::other(error.to_string())
}

/// `expr[index]` against PostgreSQL's one-based indexing.
///
/// An array value is indexed directly; the text form PostgreSQL's arrays use
/// (`{1,2,3}`) is parsed on the fly, which is how clients pick elements out of
/// the catalogue's text-typed `oidvector`/`int2vector` columns. A `NULL`
/// subscript, a `NULL` operand and an out-of-range index all produce `NULL`,
/// the way PostgreSQL's array access does.
fn eval_subscript(operand: &Value, index: &Value) -> Result<Value, ExecError> {
    let index = match index {
        Value::Null => return Ok(Value::Null),
        Value::Int2(n) => *n as i64,
        Value::Int4(n) => *n as i64,
        Value::Int8(n) => *n,
        other => {
            return Err(ExecError::other(format!(
                "array subscript must be an integer, not {}",
                other.type_of()
            )))
        }
    };
    let items = match operand {
        Value::Null => return Ok(Value::Null),
        Value::Array(items) => items.clone(),
        Value::Text(text) => parse_array_literal(text).ok_or_else(|| {
            ExecError::other(format!("cannot subscript type {}", operand.type_of()))
        })?,
        other => {
            return Err(ExecError::other(format!(
                "cannot subscript type {}",
                other.type_of()
            )))
        }
    };
    if index < 1 {
        // Zero and negative subscripts address the extra dimensions an
        // in-memory array here cannot carry; nothing sensible to return.
        return Ok(Value::Null);
    }
    Ok(items
        .get((index - 1) as usize)
        .cloned()
        .unwrap_or(Value::Null))
}

/// Splits the text form of a one-dimensional PostgreSQL array into its
/// elements, honouring quotes and escapes; `None` when `text` is not one.
fn parse_array_literal(text: &str) -> Option<Vec<Value>> {
    let inner = text.strip_prefix('{')?.strip_suffix('}')?;
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut items = Vec::new();
    let mut item = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut seen = false;
    for ch in inner.chars() {
        if escaped {
            item.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => {
                quoted = !quoted;
                seen = true;
            }
            ',' if !quoted => {
                items.push(array_element(item.trim(), seen));
                item.clear();
                seen = false;
            }
            _ => {
                item.push(ch);
                seen = true;
            }
        }
    }
    if quoted {
        return None;
    }
    items.push(array_element(item.trim(), seen));
    Some(items)
}

/// One literal element: an unquoted empty string or `NULL` is SQL `NULL`.
fn array_element(raw: &str, quoted: bool) -> Value {
    if !quoted && (raw.is_empty() || raw.eq_ignore_ascii_case("null")) {
        Value::Null
    } else {
        Value::Text(raw.to_string())
    }
}

/// SQL `LIKE` matching.
///
/// `%` matches any sequence, `_` matches one character, `\` escapes the next
/// character (PostgreSQL's default), and `ILIKE` folds both sides with
/// `to_lowercase` — the same approximation PostgreSQL makes for non-`C`
/// collations in an in-memory engine.
pub fn like_match(value: &str, pattern: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        // Folding produces owned text, so only `ILIKE` pays for it.
        let folded_value = value.to_lowercase();
        let folded_pattern = pattern.to_lowercase();
        return like_recursive(&chars(&folded_value), &chars(&folded_pattern));
    }
    like_recursive(&chars(value), &chars(pattern))
}

/// Splits text into the characters the matcher walks.
fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn like_recursive(value: &[char], pattern: &[char]) -> bool {
    if pattern.is_empty() {
        return value.is_empty();
    }
    match pattern[0] {
        '%' => {
            // Collapse consecutive wildcards to keep the recursion shallow.
            let rest = &pattern[1..];
            if like_recursive(value, rest) {
                return true;
            }
            for index in 0..value.len() {
                if like_recursive(&value[index + 1..], rest) {
                    return true;
                }
            }
            false
        }
        '_' => !value.is_empty() && like_recursive(&value[1..], &pattern[1..]),
        '\\' if pattern.len() > 1 => {
            !value.is_empty()
                && value[0] == pattern[1]
                && like_recursive(&value[1..], &pattern[2..])
        }
        other => {
            !value.is_empty() && value[0] == other && like_recursive(&value[1..], &pattern[1..])
        }
    }
}

/// Converts a decimal to the narrowest integer type that holds it.
pub fn decimal_to_int(value: Decimal, data_type: &DataType) -> Result<Value, ExecError> {
    let Some(int) = value.to_i64() else {
        return Err(ExecError::other("value out of range"));
    };
    let narrowed = Value::Int8(int).cast_to(data_type)?;
    Ok(narrowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn eval(expr: &ScalarExpr) -> Value {
        expr.eval(&Row::new(), &EvalEnv::default())
            .expect("evaluation must succeed")
    }

    #[test]
    fn three_valued_logic_follows_sql() {
        let null = ScalarExpr::Const(Value::Null);
        let truth = ScalarExpr::Const(Value::Bool(true));
        let falsity = ScalarExpr::Const(Value::Bool(false));

        // `NULL AND FALSE` is FALSE, `NULL AND TRUE` is NULL.
        let and = |left: &ScalarExpr, right: &ScalarExpr| ScalarExpr::Binary {
            op: BinaryOp::And,
            left: Box::new(left.clone()),
            right: Box::new(right.clone()),
        };
        assert_eq!(eval(&and(&null, &falsity)), Value::Bool(false));
        assert_eq!(eval(&and(&null, &truth)), Value::Null);

        // `NULL OR TRUE` is TRUE, `NULL OR FALSE` is NULL.
        let or = |left: &ScalarExpr, right: &ScalarExpr| ScalarExpr::Binary {
            op: BinaryOp::Or,
            left: Box::new(left.clone()),
            right: Box::new(right.clone()),
        };
        assert_eq!(eval(&or(&null, &truth)), Value::Bool(true));
        assert_eq!(eval(&or(&null, &falsity)), Value::Null);
    }

    #[test]
    fn comparisons_propagate_null() {
        let compare = |left: Value, right: Value| ScalarExpr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(ScalarExpr::Const(left)),
            right: Box::new(ScalarExpr::Const(right)),
        };
        assert_eq!(
            eval(&compare(Value::Int4(1), Value::Int4(1))),
            Value::Bool(true)
        );
        // `1 = NULL` is unknown, not false.
        assert_eq!(eval(&compare(Value::Int4(1), Value::Null)), Value::Null);
    }

    #[test]
    fn is_distinct_from_never_returns_null() {
        let distinct = |left: Value, right: Value| ScalarExpr::IsDistinctFrom {
            left: Box::new(ScalarExpr::Const(left)),
            right: Box::new(ScalarExpr::Const(right)),
            negated: false,
        };
        assert_eq!(
            eval(&distinct(Value::Null, Value::Null)),
            Value::Bool(false)
        );
        assert_eq!(
            eval(&distinct(Value::Null, Value::Int4(1))),
            Value::Bool(true)
        );
        assert_eq!(
            eval(&distinct(Value::Int4(1), Value::Int4(1))),
            Value::Bool(false)
        );
    }

    #[test]
    fn arithmetic_widens_and_detects_division_by_zero() {
        let binary = |op: BinaryOp, left: Value, right: Value| ScalarExpr::Binary {
            op,
            left: Box::new(ScalarExpr::Const(left)),
            right: Box::new(ScalarExpr::Const(right)),
        };
        // Integer division truncates, exactly like PostgreSQL's `/`.
        assert_eq!(
            eval(&binary(BinaryOp::Divide, Value::Int8(7), Value::Int8(2))),
            Value::Int8(3)
        );
        assert_eq!(
            eval(&binary(BinaryOp::Plus, Value::Int8(1), Value::Float8(0.5))),
            Value::Float8(1.5)
        );
        let error = binary(BinaryOp::Divide, Value::Int8(1), Value::Int8(0))
            .eval(&Row::new(), &EvalEnv::default())
            .unwrap_err();
        assert_eq!(error, ExecError::DivisionByZero);
    }

    #[test]
    fn like_supports_wildcards_and_escapes() {
        assert!(like_match("abc", "a%", false));
        assert!(like_match("abc", "a_c", false));
        assert!(!like_match("abc", "b%", false));
        assert!(like_match("ABC", "a%", true), "ILIKE folds case");
        assert!(!like_match("ABC", "a%", false));
        // A backslash escapes the wildcard.
        assert!(like_match("a%c", "a\\%c", false));
        assert!(!like_match("abc", "a\\%c", false));
    }

    #[test]
    fn out_of_range_slots_are_reported_not_panicked() {
        let error = ScalarExpr::Column(7)
            .eval(&Row::new(), &EvalEnv::default())
            .unwrap_err();
        assert!(error.to_string().contains("out of range"), "{error}");
    }

    #[test]
    fn correlated_references_walk_the_environment() {
        let env = EvalEnv {
            statement_time: Some(chrono::Utc.timestamp_opt(0, 0).unwrap()),
            ..EvalEnv::default()
        };
        let inner = env.push(&vec![Value::Int4(42)]);
        let expr = ScalarExpr::Outer {
            levels_up: 1,
            index: 0,
        };
        assert_eq!(expr.eval(&Row::new(), &inner).unwrap(), Value::Int4(42));
        // One level too far is an error, not a panic.
        let too_far = ScalarExpr::Outer {
            levels_up: 2,
            index: 0,
        };
        assert!(too_far.eval(&Row::new(), &inner).is_err());
    }
}
