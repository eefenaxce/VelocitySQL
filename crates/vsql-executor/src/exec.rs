//! The volcano-style executor.
//!
//! Every plan node becomes one [`Operator`] with a `next()` method. Operators
//! stream whenever the semantics allow it (scan, filter, project, limit, join)
//! and materialise only when they must (sort, aggregate, distinct, set
//! operations, and the inner side of a nested-loop join).
//!
//! Streaming matters even in an in-memory engine: `LIMIT 10` over a ten-million
//! row scan must not copy ten million rows, and the same property is what lets
//! milestone M3 push results to a client as they are produced.

use std::collections::{HashMap, HashSet};

use vsql_parser::JoinType;
use vsql_storage::RowId;
use vsql_types::{Row, Value};

use crate::catalog_scan;
use crate::error::ExecError;
use crate::expr::{EvalEnv, ScalarExpr};
use crate::field::{Field, Schema};
use crate::plan::{
    AggregateCall, AggregateFunc, Plan, SetOperator, SortKey, TableFunction, TableFunctionPlan,
};

/// A pull-based operator.
pub trait Operator {
    /// The schema of the rows this operator produces.
    fn schema(&self) -> &[Field];
    /// Produces the next row, or `None` at end of stream.
    fn next(&mut self) -> Result<Option<Row>, ExecError>;
}

/// Builds an operator tree for `plan`.
///
/// `env` carries the enclosing rows so that operators holding correlated
/// expressions can evaluate them.
pub fn build(plan: &Plan, env: &EvalEnv) -> Result<Box<dyn Operator>, ExecError> {
    let operator: Box<dyn Operator> = match plan {
        Plan::SingleRow { schema } => Box::new(SingleRowOp {
            schema: schema.clone(),
            done: false,
        }),
        Plan::Values { rows, schema } => {
            let mut evaluated = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values = Vec::with_capacity(row.len());
                for expr in row {
                    values.push(expr.eval(&Row::new(), env)?);
                }
                evaluated.push(values);
            }
            Box::new(ValuesOp {
                schema: schema.clone(),
                rows: evaluated,
                position: 0,
            })
        }
        Plan::TableFunction(function) => {
            // The arguments are constant for the query, so the rows are
            // materialised once while the plan is built.
            Box::new(ValuesOp {
                schema: function.schema.clone(),
                rows: eval_table_function(function, env)?,
                position: 0,
            })
        }
        Plan::Scan(scan) => {
            let rids = resolve_scan_rids(scan, env)?;
            Box::new(ScanOp {
                table: scan.table.clone(),
                schema: scan.schema.clone(),
                columns: scan.columns.clone(),
                filter: scan.filter.clone(),
                rids,
                position: 0,
                env: env.clone(),
            })
        }
        Plan::CatalogScan(scan) => {
            // The snapshot is materialised once per statement, so every
            // `pg_catalog` relation in the same statement sees one state.
            let snapshot = env.catalog.as_deref().ok_or_else(|| {
                ExecError::other("system catalogue queries require a session context")
            })?;
            Box::new(ValuesOp {
                schema: scan.schema.clone(),
                rows: catalog_scan::rows(snapshot, scan.source),
                position: 0,
            })
        }
        Plan::Alias { input, schema } => Box::new(AliasOp {
            input: build(input, env)?,
            schema: schema.clone(),
        }),
        Plan::Filter { input, predicate } => Box::new(FilterOp {
            input: build(input, env)?,
            predicate: predicate.clone(),
            env: env.clone(),
        }),
        Plan::Project {
            input,
            exprs,
            schema,
        } => Box::new(ProjectOp {
            input: build(input, env)?,
            exprs: exprs.clone(),
            schema: schema.clone(),
            env: env.clone(),
        }),
        Plan::Sort { input, keys } => Box::new(SortOp {
            input: build(input, env)?,
            keys: keys.clone(),
            env: env.clone(),
            rows: None,
            position: 0,
        }),
        Plan::Limit {
            input,
            limit,
            offset,
        } => Box::new(LimitOp {
            input: build(input, env)?,
            limit: *limit,
            offset: *offset,
            seen: 0,
            emitted: 0,
        }),
        Plan::Join {
            left,
            right,
            join_type,
            on,
            schema,
        } => Box::new(JoinOp {
            left: build(left, env)?,
            right: build(right, env)?,
            join_type: *join_type,
            on: on.clone(),
            schema: schema.clone(),
            env: env.clone(),
            left_rows: Vec::new(),
            right_rows: Vec::new(),
            left_width: left.schema().len(),
            right_width: right.schema().len(),
            left_index: 0,
            right_index: 0,
            right_matched: Vec::new(),
            left_matched: false,
            loaded: false,
        }),
        Plan::Aggregate {
            input,
            groups,
            aggregates,
            schema,
        } => Box::new(AggregateOp {
            input: build(input, env)?,
            groups: groups.clone(),
            aggregates: aggregates.clone(),
            schema: schema.clone(),
            env: env.clone(),
            rows: None,
            position: 0,
        }),
        Plan::Distinct { input, on } => Box::new(DistinctOp {
            input: build(input, env)?,
            on: on.clone(),
            env: env.clone(),
            rows: None,
            position: 0,
        }),
        Plan::SetOp {
            left,
            right,
            op,
            all,
            schema,
        } => Box::new(SetOpOp {
            left: build(left, env)?,
            right: build(right, env)?,
            op: *op,
            all: *all,
            schema: schema.clone(),
            env: env.clone(),
            rows: None,
            position: 0,
        }),
    };
    Ok(operator)
}

/// Runs a plan to completion and returns every row.
///
/// Used by subquery evaluation, by DML sources and by the tests; the streaming
/// path ([`build`]) is what `SELECT` uses.
pub fn run_plan(plan: &Plan, env: &EvalEnv) -> Result<Vec<Row>, ExecError> {
    let mut operator = build(plan, env)?;
    let mut rows = Vec::new();
    while let Some(row) = operator.next()? {
        rows.push(row);
    }
    Ok(rows)
}

/// Materialises the rows of a FROM-level set-returning function.
fn eval_table_function(function: &TableFunctionPlan, env: &EvalEnv) -> Result<Vec<Row>, ExecError> {
    let row = Row::new();
    match &function.function {
        TableFunction::GenerateSeries { start, stop, step } => {
            let start = series_bound(start.eval(&row, env)?)?;
            let stop = series_bound(stop.eval(&row, env)?)?;
            let step = series_bound(step.eval(&row, env)?)?;
            if step == 0 {
                return Err(ExecError::Other("step size cannot equal zero".to_string()));
            }
            let mut rows = Vec::new();
            let mut value = start;
            while if step > 0 {
                value <= stop
            } else {
                value >= stop
            } {
                rows.push(vec![Value::Int8(value)]);
                value += step;
            }
            Ok(rows)
        }
        TableFunction::Keywords => Ok(vsql_parser::keywords()
            .map(|(word, reserved)| {
                vec![
                    Value::Text(word.to_string()),
                    Value::Text(if reserved { "R" } else { "U" }.to_string()),
                    Value::Text(if reserved { "reserved" } else { "unreserved" }.to_string()),
                    Value::Bool(!reserved),
                ]
            })
            .collect()),
        // No extension ships with the engine, so both catalogues are empty.
        TableFunction::AvailableExtensions | TableFunction::AvailableExtensionVersions => {
            Ok(Vec::new())
        }
        TableFunction::Unnest { array } => match array.eval(&row, env)? {
            Value::Array(items) => Ok(items.into_iter().map(|item| vec![item]).collect()),
            // A bind parameter arrives as text, and `unnest` is the one place
            // where that text can only be an array literal.
            Value::Text(text) => match Value::parse_array_literal(&text) {
                Some(items) => Ok(items.into_iter().map(|item| vec![item]).collect()),
                None => Ok(vec![vec![Value::Text(text)]]),
            },
            Value::Null => Ok(Vec::new()),
            other => Ok(vec![vec![other]]),
        },
    }
}

/// Reads a generate_series bound, which PostgreSQL accepts as a plain integer.
fn series_bound(value: Value) -> Result<i64, ExecError> {
    match value {
        Value::Int2(value) => Ok(value as i64),
        Value::Int4(value) => Ok(value as i64),
        Value::Int8(value) => Ok(value),
        other => Err(ExecError::UndefinedFunction(format!(
            "generate_series({})",
            other.type_of().format_type()
        ))),
    }
}

/// Computes the row identifiers a scan should visit.
///
/// An equality probe on an index is evaluated here, once, rather than per row —
/// index selection is a plan-time decision, so its cost belongs to plan set-up.
fn resolve_scan_rids(scan: &crate::plan::ScanPlan, env: &EvalEnv) -> Result<Vec<RowId>, ExecError> {
    if let Some(probe) = &scan.index {
        let mut key = Vec::with_capacity(probe.keys.len());
        let mut complete = true;
        for expr in &probe.keys {
            let value = expr.eval(&Row::new(), env)?;
            if value.is_null() {
                // `col = NULL` matches nothing, and that is not an error.
                complete = false;
                break;
            }
            key.push(value);
        }
        if !complete {
            return Ok(Vec::new());
        }
        return Ok(scan
            .table
            .index_lookup(probe.index.oid, &key)
            .unwrap_or_default());
    }
    Ok(scan.table.rids())
}

// ------------------------------------------------------------------ operators

struct SingleRowOp {
    schema: Schema,
    done: bool,
}

impl Operator for SingleRowOp {
    fn schema(&self) -> &[Field] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Row::new()))
    }
}

struct ValuesOp {
    schema: Schema,
    rows: Vec<Row>,
    position: usize,
}

impl Operator for ValuesOp {
    fn schema(&self) -> &[Field] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        let row = self.rows.get(self.position).cloned();
        self.position += 1;
        Ok(row)
    }
}

struct ScanOp {
    table: std::sync::Arc<vsql_storage::Table>,
    schema: Schema,
    columns: Vec<usize>,
    filter: Option<ScalarExpr>,
    rids: Vec<RowId>,
    position: usize,
    env: EvalEnv,
}

impl Operator for ScanOp {
    fn schema(&self) -> &[Field] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        while let Some(rid) = self.rids.get(self.position).copied() {
            self.position += 1;
            // Projecting inside the table's read guard copies the requested
            // columns and nothing else; cloning the stored row first would copy
            // every column of every row the scan touches.
            let projected = self.table.with_row(rid, |stored| -> Row {
                self.columns
                    .iter()
                    .map(|index| stored.get(*index).cloned().unwrap_or(Value::Null))
                    .collect()
            });
            let Some(projected) = projected else {
                // The row was deleted after the scan snapshot was taken.
                continue;
            };
            if let Some(filter) = &self.filter {
                if !matches!(filter.eval(&projected, &self.env)?.as_bool(), Some(true)) {
                    continue;
                }
            }
            return Ok(Some(projected));
        }
        Ok(None)
    }
}

struct AliasOp {
    input: Box<dyn Operator>,
    schema: Schema,
}

impl Operator for AliasOp {
    fn schema(&self) -> &[Field] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        self.input.next()
    }
}

struct FilterOp {
    input: Box<dyn Operator>,
    predicate: ScalarExpr,
    env: EvalEnv,
}

impl Operator for FilterOp {
    fn schema(&self) -> &[Field] {
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        while let Some(row) = self.input.next()? {
            if matches!(self.predicate.eval(&row, &self.env)?.as_bool(), Some(true)) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

struct ProjectOp {
    input: Box<dyn Operator>,
    exprs: Vec<ScalarExpr>,
    schema: Schema,
    env: EvalEnv,
}

impl Operator for ProjectOp {
    fn schema(&self) -> &[Field] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        let Some(row) = self.input.next()? else {
            return Ok(None);
        };
        let mut projected = Vec::with_capacity(self.exprs.len());
        for expr in &self.exprs {
            projected.push(expr.eval(&row, &self.env)?);
        }
        Ok(Some(projected))
    }
}

struct SortOp {
    input: Box<dyn Operator>,
    keys: Vec<SortKey>,
    env: EvalEnv,
    rows: Option<Vec<Row>>,
    position: usize,
}

impl Operator for SortOp {
    fn schema(&self) -> &[Field] {
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        if self.rows.is_none() {
            let mut decorated = Vec::new();
            while let Some(row) = self.input.next()? {
                let mut key = Vec::with_capacity(self.keys.len());
                for sort_key in &self.keys {
                    key.push(sort_key.expr.eval(&row, &self.env)?);
                }
                decorated.push((key, row));
            }
            // Sorting with pre-computed keys keeps the comparator cheap: it
            // compares values, never re-evaluates expressions.
            decorated.sort_by(|(left, _), (right, _)| {
                for (index, sort_key) in self.keys.iter().enumerate() {
                    let ordering = compare_sort_values(
                        &left[index],
                        &right[index],
                        sort_key.asc,
                        sort_key.nulls_first,
                    );
                    if ordering != std::cmp::Ordering::Equal {
                        return ordering;
                    }
                }
                std::cmp::Ordering::Equal
            });
            self.rows = Some(decorated.into_iter().map(|(_, row)| row).collect());
        }
        let rows = self.rows.as_ref().expect("populated above");
        let row = rows.get(self.position).cloned();
        self.position += 1;
        Ok(row)
    }
}

/// PostgreSQL's `ORDER BY` ordering: `ASC` puts `NULL`s last by default and
/// `DESC` puts them first, and an explicit `NULLS FIRST/LAST` wins.
fn compare_sort_values(
    left: &Value,
    right: &Value,
    asc: bool,
    nulls_first: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let ordering = left.cmp(right);
            if asc {
                ordering
            } else {
                ordering.reverse()
            }
        }
    }
}

struct LimitOp {
    input: Box<dyn Operator>,
    limit: Option<u64>,
    offset: u64,
    seen: u64,
    emitted: u64,
}

impl Operator for LimitOp {
    fn schema(&self) -> &[Field] {
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        if let Some(limit) = self.limit {
            if self.emitted >= limit {
                return Ok(None);
            }
        }
        loop {
            let Some(row) = self.input.next()? else {
                return Ok(None);
            };
            if self.seen < self.offset {
                self.seen += 1;
                continue;
            }
            self.emitted += 1;
            return Ok(Some(row));
        }
    }
}

struct JoinOp {
    left: Box<dyn Operator>,
    right: Box<dyn Operator>,
    join_type: JoinType,
    on: Option<ScalarExpr>,
    schema: Schema,
    env: EvalEnv,
    left_rows: Vec<Row>,
    right_rows: Vec<Row>,
    left_width: usize,
    right_width: usize,
    left_index: usize,
    right_index: usize,
    right_matched: Vec<bool>,
    left_matched: bool,
    loaded: bool,
}

impl JoinOp {
    /// Drains both inputs.
    ///
    /// The right side has to be re-read once per left row, so buffering it is
    /// what turns the nested loop into a predictable O(n·m) scan instead of an
    /// O(n) plan rebuild. Milestone M5 replaces this with a hash join.
    fn load(&mut self) -> Result<(), ExecError> {
        while let Some(row) = self.left.next()? {
            self.left_rows.push(row);
        }
        while let Some(row) = self.right.next()? {
            self.right_rows.push(row);
        }
        self.right_matched = vec![false; self.right_rows.len()];
        self.loaded = true;
        Ok(())
    }

    fn concat(left: &Row, right: &Row) -> Row {
        let mut row = Vec::with_capacity(left.len() + right.len());
        row.extend_from_slice(left);
        row.extend_from_slice(right);
        row
    }
}

impl Operator for JoinOp {
    fn schema(&self) -> &[Field] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        if !self.loaded {
            self.load()?;
        }
        loop {
            if self.left_index >= self.left_rows.len() {
                // Unmatched right rows only matter for RIGHT/FULL joins.
                if matches!(self.join_type, JoinType::Right | JoinType::Full) {
                    while self.right_index < self.right_rows.len() {
                        let index = self.right_index;
                        self.right_index += 1;
                        if !self.right_matched[index] {
                            let left = vec![Value::Null; self.left_width];
                            return Ok(Some(Self::concat(&left, &self.right_rows[index])));
                        }
                    }
                }
                return Ok(None);
            }

            let left_row = self.left_rows[self.left_index].clone();
            while self.right_index < self.right_rows.len() {
                let index = self.right_index;
                self.right_index += 1;
                let combined = Self::concat(&left_row, &self.right_rows[index]);
                let matched = match &self.on {
                    None => true,
                    Some(predicate) => {
                        matches!(predicate.eval(&combined, &self.env)?.as_bool(), Some(true))
                    }
                };
                if matched {
                    self.left_matched = true;
                    if matches!(self.join_type, JoinType::Right | JoinType::Full) {
                        self.right_matched[index] = true;
                    }
                    return Ok(Some(combined));
                }
            }

            let emit_left =
                !self.left_matched && matches!(self.join_type, JoinType::Left | JoinType::Full);
            self.left_index += 1;
            self.right_index = 0;
            self.left_matched = false;
            if emit_left {
                let right = vec![Value::Null; self.right_width];
                return Ok(Some(Self::concat(&left_row, &right)));
            }
        }
    }
}

struct AggregateOp {
    input: Box<dyn Operator>,
    groups: Vec<ScalarExpr>,
    aggregates: Vec<AggregateCall>,
    schema: Schema,
    env: EvalEnv,
    rows: Option<Vec<Row>>,
    position: usize,
}

impl Operator for AggregateOp {
    fn schema(&self) -> &[Field] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        if self.rows.is_none() {
            self.rows = Some(self.aggregate()?);
        }
        let rows = self.rows.as_ref().expect("populated above");
        let row = rows.get(self.position).cloned();
        self.position += 1;
        Ok(row)
    }
}

impl AggregateOp {
    fn aggregate(&mut self) -> Result<Vec<Row>, ExecError> {
        // Hash grouping: the key is the tuple of grouping expressions, which is
        // exactly what `Value`'s `Eq`/`Hash` implement.
        let mut positions: HashMap<Row, usize> = HashMap::new();
        let mut keys: Vec<Row> = Vec::new();
        let mut states: Vec<Vec<AggregateState>> = Vec::new();

        while let Some(row) = self.input.next()? {
            let mut key = Vec::with_capacity(self.groups.len());
            for group in &self.groups {
                key.push(group.eval(&row, &self.env)?);
            }
            let index = match positions.get(&key) {
                Some(index) => *index,
                None => {
                    let index = keys.len();
                    positions.insert(key.clone(), index);
                    keys.push(key);
                    states.push(self.aggregates.iter().map(AggregateState::new).collect());
                    index
                }
            };
            if states[index].is_empty() {
                states[index] = self.aggregates.iter().map(AggregateState::new).collect();
            }
            for (position, call) in self.aggregates.iter().enumerate() {
                let mut argument = None;
                if let Some(arg) = &call.arg {
                    argument = Some(arg.eval(&row, &self.env)?);
                }
                let mut delimiter = None;
                if let Some(expr) = &call.delimiter {
                    delimiter = Some(expr.eval(&row, &self.env)?);
                }
                let keep = match &call.filter {
                    None => true,
                    Some(filter) => {
                        matches!(filter.eval(&row, &self.env)?.as_bool(), Some(true))
                    }
                };
                if !keep {
                    continue;
                }
                states[index][position].update(call, argument, delimiter)?;
            }
        }

        // A query with aggregates but no `GROUP BY` always returns exactly one
        // row, even when the input is empty (`SELECT count(*) FROM empty`).
        if self.groups.is_empty() && keys.is_empty() {
            keys.push(Row::new());
            states.push(self.aggregates.iter().map(AggregateState::new).collect());
        }

        let mut out = Vec::with_capacity(keys.len());
        for (index, key) in keys.into_iter().enumerate() {
            let mut row = key;
            for (position, call) in self.aggregates.iter().enumerate() {
                row.push(std::mem::take(&mut states[index][position]).finish(call.func)?);
            }
            out.push(row);
        }
        Ok(out)
    }
}

/// Accumulator for one aggregate within one group.
#[derive(Debug, Default)]
struct AggregateState {
    seen: HashSet<Value>,
    count: i64,
    total: Option<Value>,
    extreme: Option<Value>,
}

impl AggregateState {
    fn new(_call: &AggregateCall) -> AggregateState {
        AggregateState::default()
    }

    /// Folds one value into the accumulator.
    ///
    /// `DISTINCT` is applied before aggregation, mirroring PostgreSQL's
    /// `count(DISTINCT x)`: repeated values never reach the accumulator.
    fn update(
        &mut self,
        call: &AggregateCall,
        argument: Option<Value>,
        delimiter: Option<Value>,
    ) -> Result<(), ExecError> {
        let func = call.func;
        if func == AggregateFunc::CountStar {
            self.count += 1;
            return Ok(());
        }
        let Some(value) = argument else {
            return Ok(());
        };
        if value.is_null() {
            return Ok(());
        }
        if call.distinct && !self.seen.insert(value.clone()) {
            return Ok(());
        }
        match func {
            AggregateFunc::Count => {
                self.count += 1;
            }
            AggregateFunc::Sum | AggregateFunc::Avg => {
                self.count += 1;
                self.total = Some(match self.total.take() {
                    None => value,
                    Some(total) => {
                        crate::expr::eval_binary(vsql_parser::BinaryOp::Plus, &total, &value)?
                    }
                });
            }
            AggregateFunc::Min | AggregateFunc::Max => {
                let better = match &self.extreme {
                    None => true,
                    Some(current) => {
                        let ordering = value
                            .compare(current, if func == AggregateFunc::Min { "<" } else { ">" })?
                            .unwrap_or(std::cmp::Ordering::Equal);
                        if func == AggregateFunc::Min {
                            ordering == std::cmp::Ordering::Less
                        } else {
                            ordering == std::cmp::Ordering::Greater
                        }
                    }
                };
                if better {
                    self.extreme = Some(value);
                }
            }
            AggregateFunc::StringAgg => {
                // The text forms are PostgreSQL's output forms (`t` for true,
                // `2024-01-02` for dates), and a `NULL` delimiter glues parts
                // together without a separator, as in PostgreSQL.
                let piece = value.to_string();
                let glue = delimiter.filter(|d| !d.is_null()).map(|d| d.to_string());
                let existing = self.total.take();
                let first = existing.is_none();
                let mut text = match existing {
                    Some(Value::Text(text)) => text,
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                // The delimiter goes *between* parts. Extending the accumulated
                // text in place keeps the whole aggregation linear; rebuilding
                // it with `format!` per value copies every part again.
                if !first {
                    if let Some(glue) = glue {
                        text.push_str(&glue);
                    }
                }
                text.push_str(&piece);
                self.total = Some(Value::Text(text));
            }
            AggregateFunc::CountStar => unreachable!("handled above"),
        }
        Ok(())
    }

    fn finish(self, func: AggregateFunc) -> Result<Value, ExecError> {
        let value = match func {
            AggregateFunc::Count | AggregateFunc::CountStar => Value::Int8(self.count),
            AggregateFunc::Sum => self.total.unwrap_or(Value::Null),
            AggregateFunc::Avg => match self.total {
                None => Value::Null,
                Some(total) => {
                    if self.count == 0 {
                        Value::Null
                    } else {
                        crate::expr::eval_binary(
                            vsql_parser::BinaryOp::Divide,
                            &total,
                            &Value::Int8(self.count),
                        )?
                    }
                }
            },
            AggregateFunc::Min | AggregateFunc::Max => self.extreme.unwrap_or(Value::Null),
            AggregateFunc::StringAgg => self.total.unwrap_or(Value::Null),
        };
        Ok(value)
    }
}

struct DistinctOp {
    input: Box<dyn Operator>,
    on: Vec<ScalarExpr>,
    env: EvalEnv,
    rows: Option<Vec<Row>>,
    position: usize,
}

impl Operator for DistinctOp {
    fn schema(&self) -> &[Field] {
        self.input.schema()
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        if self.rows.is_none() {
            let mut seen: HashSet<Row> = HashSet::new();
            let mut out = Vec::new();
            while let Some(row) = self.input.next()? {
                let key: Row = self
                    .on
                    .iter()
                    .map(|expr| expr.eval(&row, &self.env))
                    .collect::<Result<Vec<_>, _>>()?;
                if seen.insert(key) {
                    out.push(row);
                }
            }
            self.rows = Some(out);
        }
        let rows = self.rows.as_ref().expect("populated above");
        let row = rows.get(self.position).cloned();
        self.position += 1;
        Ok(row)
    }
}

struct SetOpOp {
    left: Box<dyn Operator>,
    right: Box<dyn Operator>,
    op: SetOperator,
    all: bool,
    schema: Schema,
    env: EvalEnv,
    rows: Option<Vec<Row>>,
    position: usize,
}

impl Operator for SetOpOp {
    fn schema(&self) -> &[Field] {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Row>, ExecError> {
        if self.rows.is_none() {
            self.rows = Some(self.combine()?);
        }
        let rows = self.rows.as_ref().expect("populated above");
        let row = rows.get(self.position).cloned();
        self.position += 1;
        Ok(row)
    }
}

impl SetOpOp {
    fn combine(&mut self) -> Result<Vec<Row>, ExecError> {
        let mut left = Vec::new();
        while let Some(row) = self.left.next()? {
            left.push(row);
        }
        let mut right = Vec::new();
        while let Some(row) = self.right.next()? {
            right.push(row);
        }
        let _ = &self.env;
        let out = match self.op {
            SetOperator::Union => {
                let mut rows = left;
                rows.extend(right);
                if self.all {
                    rows
                } else {
                    dedupe(rows)
                }
            }
            SetOperator::Intersect => {
                let mut counts: HashMap<Row, usize> = HashMap::new();
                for row in right {
                    *counts.entry(row).or_insert(0) += 1;
                }
                let mut emitted: HashSet<Row> = HashSet::new();
                let mut out = Vec::new();
                for row in left {
                    let Some(available) = counts.get_mut(&row) else {
                        continue;
                    };
                    if *available == 0 {
                        continue;
                    }
                    if self.all {
                        *available -= 1;
                        out.push(row);
                    } else if emitted.insert(row.clone()) {
                        out.push(row);
                    }
                }
                out
            }
            SetOperator::Except => {
                let mut counts: HashMap<Row, usize> = HashMap::new();
                for row in right {
                    *counts.entry(row).or_insert(0) += 1;
                }
                let mut emitted: HashSet<Row> = HashSet::new();
                let mut out = Vec::new();
                for row in left {
                    match counts.get_mut(&row) {
                        Some(available) if *available > 0 => {
                            if self.all {
                                *available -= 1;
                            }
                            continue;
                        }
                        _ => {}
                    }
                    if self.all || emitted.insert(row.clone()) {
                        out.push(row);
                    }
                }
                out
            }
        };
        Ok(out)
    }
}

fn dedupe(rows: Vec<Row>) -> Vec<Row> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if seen.insert(row.clone()) {
            out.push(row);
        }
    }
    out
}
