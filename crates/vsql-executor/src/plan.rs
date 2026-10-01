//! Physical plans.
//!
//! A plan is a tree of nodes with two properties that make the executor simple:
//!
//! * **Every node knows its output schema.** Column indices flow upwards, so no
//!   node ever has to ask its child what it produces.
//! * **Every expression is already bound.** `ScalarExpr::Column(n)` refers to a
//!   slot of the node's *input* row.
//!
//! These are the operators milestone M1 implements; the volcano model means
//! adding one later (hash join, window function) is a local change.

use std::sync::Arc;

use vsql_catalog::IndexDef;
use vsql_parser::JoinType;
use vsql_storage::Table;

use crate::catalog_scan::CatalogSource;
use crate::expr::{EvalEnv, ScalarExpr};
use crate::field::Schema;

/// A physical plan node.
#[derive(Clone, Debug)]
pub enum Plan {
    /// A relation with the given schema and one empty row.
    ///
    /// This is what `SELECT 1` scans: a single row of zero columns.
    SingleRow {
        /// Output schema (normally empty).
        schema: Schema,
    },
    /// A literal `VALUES` list.
    Values {
        /// Rows of constant expressions.
        rows: Vec<Vec<ScalarExpr>>,
        /// Output schema.
        schema: Schema,
    },
    /// A base-table access path.
    Scan(Box<ScanPlan>),
    /// A virtual `pg_catalog` relation.
    CatalogScan(Box<CatalogScanPlan>),
    /// A set-returning function in `FROM`.
    TableFunction(Box<TableFunctionPlan>),
    /// Re-labels its input's columns, e.g. for a `FROM (subquery) alias`.
    Alias {
        /// The input plan.
        input: Box<Plan>,
        /// The replacement schema.
        schema: Schema,
    },
    /// Keeps rows for which `predicate` evaluates to `true`.
    Filter {
        /// The input plan.
        input: Box<Plan>,
        /// The predicate.
        predicate: ScalarExpr,
    },
    /// Evaluates one expression per output column.
    Project {
        /// The input plan.
        input: Box<Plan>,
        /// The projected expressions.
        exprs: Vec<ScalarExpr>,
        /// Output schema.
        schema: Schema,
    },
    /// Sorts its input.
    Sort {
        /// The input plan.
        input: Box<Plan>,
        /// Sort keys.
        keys: Vec<SortKey>,
    },
    /// Applies `OFFSET`/`LIMIT`.
    Limit {
        /// The input plan.
        input: Box<Plan>,
        /// Maximum number of rows, `None` for "unlimited".
        limit: Option<u64>,
        /// Number of rows to skip.
        offset: u64,
    },
    /// Joins two inputs.
    Join {
        /// Left input.
        left: Box<Plan>,
        /// Right input.
        right: Box<Plan>,
        /// Join type.
        join_type: JoinType,
        /// Join condition, absent for `CROSS JOIN`.
        on: Option<ScalarExpr>,
        /// Output schema (left columns followed by right columns).
        schema: Schema,
    },
    /// Groups and aggregates.
    Aggregate {
        /// The input plan.
        input: Box<Plan>,
        /// Grouping expressions.
        groups: Vec<ScalarExpr>,
        /// Aggregated calls.
        aggregates: Vec<AggregateCall>,
        /// Output schema: group fields followed by aggregate fields.
        schema: Schema,
    },
    /// Removes duplicate rows.
    Distinct {
        /// The input plan.
        input: Box<Plan>,
        /// The expressions that define "duplicate".
        on: Vec<ScalarExpr>,
    },
    /// `UNION`/`INTERSECT`/`EXCEPT`.
    SetOp {
        /// Left input.
        left: Box<Plan>,
        /// Right input.
        right: Box<Plan>,
        /// The set operator.
        op: SetOperator,
        /// `ALL` keeps duplicates.
        all: bool,
        /// Output schema.
        schema: Schema,
    },
}

/// The set operations the executor can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOperator {
    /// `UNION`
    Union,
    /// `INTERSECT`
    Intersect,
    /// `EXCEPT`
    Except,
}

/// A base-table access path.
#[derive(Clone, Debug)]
pub struct ScanPlan {
    /// The table being read.
    pub table: Arc<Table>,
    /// Table column ordinals this scan projects, in output order.
    pub columns: Vec<usize>,
    /// Output schema.
    pub schema: Schema,
    /// Equality probe on an index, when the planner found one.
    pub index: Option<IndexScanPlan>,
    /// Predicate pushed down to the scan.
    pub filter: Option<ScalarExpr>,
}

/// A virtual system-catalog access path.
#[derive(Clone, Debug)]
pub struct CatalogScanPlan {
    /// Which catalogue relation to read.
    pub source: CatalogSource,
    /// Output schema.
    pub schema: Schema,
}

/// An index probe.
#[derive(Clone, Debug)]
pub struct IndexScanPlan {
    /// The index to use.
    pub index: Arc<IndexDef>,
    /// Key expressions, one per leading index column.
    pub keys: Vec<ScalarExpr>,
}

/// A set-returning function used as a `FROM` item.
#[derive(Clone, Debug)]
pub struct TableFunctionPlan {
    /// Which function to call.
    pub function: TableFunction,
    /// Output schema.
    pub schema: Schema,
}

/// The set-returning functions the engine evaluates.
///
/// Each one produces its rows from arguments that are constant for the query
/// (the planner rejects `LATERAL`), so the executor materialises them once.
#[derive(Clone, Debug)]
pub enum TableFunction {
    /// `generate_series(start, stop[, step])`, the integer forms.
    GenerateSeries {
        /// First value.
        start: ScalarExpr,
        /// Last value, inclusive.
        stop: ScalarExpr,
        /// Step between values, `1` when omitted.
        step: ScalarExpr,
    },
    /// `pg_get_keywords()`.
    Keywords,
    /// `pg_available_extensions()`.
    ///
    /// No extension is available in this engine, so it is always empty; it
    /// exists so that a client enumerating extensions gets an empty set
    /// instead of an error.
    AvailableExtensions,
    /// `pg_available_extension_versions()`, likewise always empty.
    AvailableExtensionVersions,
    /// `unnest(array)`.
    Unnest {
        /// The array to expand.
        array: ScalarExpr,
    },
}

impl TableFunction {
    /// The SQL name the function is called by.
    pub fn name(&self) -> &'static str {
        match self {
            TableFunction::GenerateSeries { .. } => "generate_series",
            TableFunction::Keywords => "pg_get_keywords",
            TableFunction::AvailableExtensions => "pg_available_extensions",
            TableFunction::AvailableExtensionVersions => "pg_available_extension_versions",
            TableFunction::Unnest { .. } => "unnest",
        }
    }
}

/// One `ORDER BY` key.
#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    /// The key expression, evaluated against the sort's input row.
    pub expr: ScalarExpr,
    /// `true` for `ASC`.
    pub asc: bool,
    /// `true` for `NULLS FIRST`.
    pub nulls_first: bool,
}

/// One aggregate call.
#[derive(Clone, Debug, PartialEq)]
pub struct AggregateCall {
    /// Which aggregate.
    pub func: AggregateFunc,
    /// The argument, absent for `COUNT(*)`.
    pub arg: Option<ScalarExpr>,
    /// The second argument of `string_agg(value, delimiter)`.
    pub delimiter: Option<ScalarExpr>,
    /// `DISTINCT` inside the call.
    pub distinct: bool,
    /// `FILTER (WHERE ...)`.
    pub filter: Option<ScalarExpr>,
}

/// The aggregates milestone M1 implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateFunc {
    /// `count(expr)`
    Count,
    /// `count(*)`
    CountStar,
    /// `sum(expr)`
    Sum,
    /// `avg(expr)`
    Avg,
    /// `min(expr)`
    Min,
    /// `max(expr)`
    Max,
    /// `string_agg(value, delimiter)`
    StringAgg,
}

impl AggregateFunc {
    /// The name PostgreSQL prints in `EXPLAIN`.
    pub fn name(&self) -> &'static str {
        match self {
            AggregateFunc::Count | AggregateFunc::CountStar => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Avg => "avg",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
            AggregateFunc::StringAgg => "string_agg",
        }
    }

    /// The type of the aggregate's result.
    pub fn result_type(&self, argument: Option<&vsql_types::DataType>) -> vsql_types::DataType {
        use vsql_types::DataType;
        match self {
            AggregateFunc::Count | AggregateFunc::CountStar => DataType::Int8,
            AggregateFunc::Avg => DataType::Numeric {
                precision: None,
                scale: None,
            },
            AggregateFunc::Sum => match argument {
                Some(DataType::Int2) | Some(DataType::Int4) | Some(DataType::Int8) => {
                    DataType::Int8
                }
                Some(DataType::Float4) | Some(DataType::Float8) => DataType::Float8,
                _ => DataType::Numeric {
                    precision: None,
                    scale: None,
                },
            },
            AggregateFunc::Min | AggregateFunc::Max => argument.cloned().unwrap_or(DataType::Text),
            AggregateFunc::StringAgg => DataType::Text,
        }
    }
}

impl Plan {
    /// The output schema of this node.
    pub fn schema(&self) -> &[crate::Field] {
        match self {
            Plan::SingleRow { schema }
            | Plan::Values { schema, .. }
            | Plan::Alias { schema, .. }
            | Plan::Project { schema, .. }
            | Plan::Join { schema, .. }
            | Plan::Aggregate { schema, .. }
            | Plan::SetOp { schema, .. } => schema,
            Plan::Scan(scan) => &scan.schema,
            Plan::CatalogScan(scan) => &scan.schema,
            Plan::TableFunction(function) => &function.schema,
            Plan::Filter { input, .. } => input.schema(),
            Plan::Sort { input, .. } => input.schema(),
            Plan::Limit { input, .. } => input.schema(),
            Plan::Distinct { input, .. } => input.schema(),
        }
    }

    /// The environment the plan's operators should be built with.
    ///
    /// Correlated subplans embed their own environment, so a plan on its own
    /// always starts from an empty one.
    pub fn empty_env() -> EvalEnv {
        EvalEnv::default()
    }
}
