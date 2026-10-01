//! The planner: turns a lowered statement into a physical plan.
//!
//! # Binding
//!
//! The central job is *name resolution*: turning `Expr::Column { qualifier, name }`
//! into a slot index. Scopes are stacked so that a subquery can see its own
//! columns (level 0) and every enclosing query block (level 1 and up, which
//! becomes [`ScalarExpr::Outer`]). That single mechanism is what makes
//! correlated subqueries work without a separate de-correlation pass.
//!
//! # Clause order
//!
//! The physical order is fixed and matches the SQL semantics:
//!
//! ```text
//! FROM → WHERE → Aggregate → HAVING → Project → Sort → Distinct → Limit
//! ```
//!
//! Projection, `ORDER BY` and `DISTINCT ON` all resolve against that same
//! projected row. Key expressions that are not selected are appended as
//! *hidden* columns and stripped afterwards, which is how
//! `SELECT a FROM t ORDER BY b` works without a second sort implementation.
//!
//! # Aggregates
//!
//! When a block groups, the projection is bound against the *aggregate output*
//! row: group keys first, aggregate results after. Two rules enforce
//! PostgreSQL's semantics:
//!
//! 1. An expression that structurally matches a group key becomes a reference to
//!    that key's slot, which is why `SELECT a + 1 ... GROUP BY a + 1` is legal.
//! 2. Any other bare column reference is rejected with PostgreSQL's
//!    `column "x" must appear in the GROUP BY clause ...` message.
//!
//! # Scope of milestone M1
//!
//! Join order is the syntactic order of the `FROM` clause and joins execute as
//! nested loops. Cost-based planning, hash joins and de-correlation arrive in
//! milestone M5; the interfaces here are what those passes will plug into.

use std::sync::Arc;

use vsql_parser::ast::{
    self, BinaryOp, Distinct, Expr, JoinConstraint, JoinType, OrderByExpr, Query, SelectItem,
    SetExpr, SetOp, TableFactor, TableWithJoins,
};
use vsql_types::{DataType, Value};

use crate::error::ExecError;
use crate::expr::{AnyAllSource, ScalarExpr, SubPlan};
use crate::field::{Field, Schema};
use crate::plan::{
    AggregateCall, AggregateFunc, IndexScanPlan, Plan, ScanPlan, SetOperator, SortKey,
    TableFunction, TableFunctionPlan,
};
use crate::Database;

/// Deepest query nesting the planner accepts before giving up.
const MAX_DEPTH: usize = 64;

/// A column visible while binding an expression.
#[derive(Clone, Debug)]
struct ScopeColumn {
    qualifier: Option<String>,
    name: String,
    data_type: DataType,
    /// Slot in the row this scope describes.
    index: usize,
    /// Hidden from `SELECT *` (the duplicate half of a `USING` join key).
    hidden: bool,
}

/// The set of columns visible at one level of the query tree.
#[derive(Clone, Debug, Default)]
struct Scope {
    columns: Vec<ScopeColumn>,
}

impl Scope {
    /// Builds a scope in which every slot of `schema` is visible.
    fn from_schema(schema: &[Field]) -> Scope {
        Scope {
            columns: schema
                .iter()
                .enumerate()
                .map(|(index, field)| ScopeColumn {
                    qualifier: field.qualifier.clone(),
                    name: field.name.clone(),
                    data_type: field.data_type.clone(),
                    index,
                    hidden: field.hidden,
                })
                .collect(),
        }
    }

    /// Builds a scope from group-key fields, none of which are hidden.
    fn from_fields(fields: Vec<Field>) -> Scope {
        Scope {
            columns: fields
                .into_iter()
                .enumerate()
                .map(|(index, field)| ScopeColumn {
                    qualifier: None,
                    name: field.name,
                    data_type: field.data_type,
                    index,
                    hidden: false,
                })
                .collect(),
        }
    }

    /// Number of slots.
    fn len(&self) -> usize {
        self.columns.len()
    }

    /// Resolves a name to a slot.
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Option<usize>, ExecError> {
        let matches: Vec<&ScopeColumn> = self
            .columns
            .iter()
            .filter(|column| {
                column.name.eq_ignore_ascii_case(name)
                    && match qualifier {
                        None => true,
                        Some(qualifier) => column
                            .qualifier
                            .as_deref()
                            .map(|candidate| candidate.eq_ignore_ascii_case(qualifier))
                            .unwrap_or(false),
                    }
            })
            .collect();
        match matches.as_slice() {
            [] => Ok(None),
            [column] => Ok(Some(column.index)),
            _ => Err(ExecError::AmbiguousColumn(name.to_string())),
        }
    }

    /// The schema of this scope.
    fn schema(&self) -> Schema {
        self.columns
            .iter()
            .map(|column| Field {
                qualifier: column.qualifier.clone(),
                name: column.name.clone(),
                data_type: column.data_type.clone(),
                hidden: column.hidden,
            })
            .collect()
    }

    /// Concatenates two scopes, shifting the right one's slots.
    fn concat(&self, other: &Scope) -> Scope {
        let offset = self.len();
        let mut columns = self.columns.clone();
        columns.extend(other.columns.iter().map(|column| ScopeColumn {
            index: column.index + offset,
            ..column.clone()
        }));
        Scope { columns }
    }
}

/// One entry of a select list, after binding.
#[derive(Clone, Debug)]
struct ProjectionItem {
    /// Source AST, used to match `ORDER BY`/`DISTINCT ON` by expression identity.
    ast: Option<Expr>,
    /// Bound expression, evaluated against the projection's input row.
    expr: ScalarExpr,
    /// Output field.
    field: Field,
    /// `true` for a column added only to serve a key.
    hidden: bool,
}

/// Per-query-block binding state.
#[derive(Default)]
struct BlockState {
    aggregates: Vec<AggregateCall>,
    aggregate_fields: Vec<Field>,
    aggregate_mode: bool,
    /// Bound grouping expressions, in output-slot order.
    group_keys: Vec<ScalarExpr>,
    /// Source ASTs of the grouping expressions, for structural matching.
    group_key_asts: Vec<Expr>,
    /// The scope an aggregate's *arguments* are bound against.
    aggregate_input_scope: Option<Scope>,
}

/// Plans statements against a single database.
pub struct Planner<'a> {
    database: &'a Database,
    /// Visible scopes, innermost first.
    scopes: Vec<Scope>,
    /// Common table expressions currently in scope.
    ctes: Vec<(String, Arc<Query>)>,
    /// Binding state of the current query block.
    block: BlockState,
    /// Recursion guard for view and CTE expansion.
    depth: usize,
}

impl<'a> Planner<'a> {
    /// Creates a planner for `database`.
    pub fn new(database: &'a Database) -> Self {
        Planner {
            database,
            scopes: Vec::new(),
            ctes: Vec::new(),
            block: BlockState::default(),
            depth: 0,
        }
    }

    /// Plans a complete query, including `ORDER BY`/`LIMIT`/`OFFSET`.
    pub fn plan_query(&mut self, query: &Query) -> Result<Plan, ExecError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(ExecError::not_supported(
                "queries nested more than 64 levels deep",
            ));
        }
        let cte_mark = self.ctes.len();
        if let Some(with) = &query.with {
            if with.recursive {
                self.depth -= 1;
                return Err(ExecError::not_supported("WITH RECURSIVE"));
            }
            for cte in &with.ctes {
                self.ctes
                    .push((cte.name.clone(), Arc::new(cte.query.clone())));
            }
        }
        let plan = match &query.body {
            SetExpr::Select(select) => self.plan_select(
                select,
                &query.order_by,
                query.limit.as_ref(),
                query.offset.as_ref(),
            ),
            other => {
                let mut plan = self.plan_set_expr(other)?;
                if !query.order_by.is_empty() {
                    plan = self.plan_output_sort(plan, &query.order_by)?;
                }
                self.plan_limit(plan, query.limit.as_ref(), query.offset.as_ref())
            }
        };
        self.ctes.truncate(cte_mark);
        self.depth -= 1;
        plan
    }

    /// Plans a query with a fresh scope and block state.
    ///
    /// Views and CTE bodies must not see the referencing query's columns, and
    /// their `GROUP BY` state must not leak into it.
    fn plan_detached_query(&mut self, query: &Query) -> Result<Plan, ExecError> {
        let saved_scopes = std::mem::take(&mut self.scopes);
        let saved_block = std::mem::take(&mut self.block);
        let plan = self.plan_query(query);
        self.scopes = saved_scopes;
        self.block = saved_block;
        plan
    }

    // ------------------------------------------------------------- body plans

    fn plan_set_expr(&mut self, body: &SetExpr) -> Result<Plan, ExecError> {
        let plan = match body {
            SetExpr::Select(select) => self.plan_select(select, &[], None, None)?,
            SetExpr::Query(query) => self.plan_query(query)?,
            SetExpr::Values(rows) => self.plan_values(rows)?,
            SetExpr::SetOperation {
                op,
                all,
                left,
                right,
            } => {
                let left_plan = self.plan_set_expr(left)?;
                let right_plan = self.plan_set_expr(right)?;
                if left_plan.schema().len() != right_plan.schema().len() {
                    return Err(ExecError::other(
                        "each set operation query must have the same number of columns",
                    ));
                }
                let op = match op {
                    SetOp::Union => SetOperator::Union,
                    SetOp::Intersect => SetOperator::Intersect,
                    SetOp::Except => SetOperator::Except,
                };
                let schema = left_plan.schema().to_vec();
                Plan::SetOp {
                    left: Box::new(left_plan),
                    right: Box::new(right_plan),
                    op,
                    all: *all,
                    schema,
                }
            }
        };
        Ok(plan)
    }

    fn plan_values(&mut self, rows: &[Vec<Expr>]) -> Result<Plan, ExecError> {
        let width = rows.first().map(Vec::len).unwrap_or(0);
        let mut bound_rows = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != width {
                return Err(ExecError::other("VALUES lists must all be the same length"));
            }
            let mut bound = Vec::with_capacity(row.len());
            for expr in row {
                bound.push(self.bind_plain(expr)?);
            }
            bound_rows.push(bound);
        }
        let schema: Schema = (0..width)
            .map(|index| {
                let data_type = bound_rows
                    .first()
                    .and_then(|row| row.get(index))
                    .and_then(|expr| expr.infer_type(&[]))
                    .unwrap_or(DataType::Text);
                Field::unqualified(format!("column{}", index + 1), data_type)
            })
            .collect();
        Ok(Plan::Values {
            rows: bound_rows,
            schema,
        })
    }

    fn plan_select(
        &mut self,
        select: &ast::Select,
        order_by: &[OrderByExpr],
        limit: Option<&Expr>,
        offset: Option<&Expr>,
    ) -> Result<Plan, ExecError> {
        // ---------------------------------------------------------- FROM
        let (mut plan, from_scope) = self.plan_from(&select.from)?;

        // --------------------------------------------------------- WHERE
        if let Some(selection) = &select.selection {
            let predicate =
                self.with_scope(from_scope.clone(), |this| this.bind_plain(selection))?;
            plan = self.apply_filter(plan, predicate, &from_scope)?;
        }

        // ------------------------------------------------- aggregate mode
        let aggregates_present =
            select.projection.iter().any(
                |item| matches!(item, SelectItem::Expr { expr, .. } if contains_aggregate(expr)),
            ) || order_by.iter().any(|item| contains_aggregate(&item.expr))
                || select
                    .having
                    .as_ref()
                    .map(contains_aggregate)
                    .unwrap_or(false);
        let aggregate_mode =
            !select.group_by.is_empty() || select.having.is_some() || aggregates_present;

        let mut items: Vec<ProjectionItem> = Vec::new();
        let mut sort_keys: Vec<SortKey> = Vec::new();
        let mut distinct_slots: Vec<usize> = Vec::new();

        if aggregate_mode {
            // Group keys are bound against the input, and become slots 0..n of
            // the aggregate output.
            let mut group_keys = Vec::with_capacity(select.group_by.len());
            let mut group_fields = Vec::with_capacity(select.group_by.len());
            for group in &select.group_by {
                if contains_aggregate(group) {
                    return Err(ExecError::AggregateNotAllowed("GROUP BY".to_string()));
                }
                let bound = self.with_scope(from_scope.clone(), |this| this.bind_plain(group))?;
                let data_type = bound
                    .infer_type(&from_scope.schema())
                    .unwrap_or_else(|| constant_type_of(group).unwrap_or(DataType::Text));
                group_fields.push(Field::unqualified(expr_name(group), data_type));
                group_keys.push(bound);
            }
            let output_scope = Scope::from_fields(group_fields.clone());
            let saved = std::mem::replace(
                &mut self.block,
                BlockState {
                    aggregate_mode: true,
                    group_keys: group_keys.clone(),
                    group_key_asts: select.group_by.clone(),
                    aggregate_input_scope: Some(from_scope.clone()),
                    aggregates: Vec::new(),
                    aggregate_fields: Vec::new(),
                },
            );

            // HAVING and the projection both see the aggregate output row.
            let having = self.with_scope(output_scope.clone(), |this| match &select.having {
                None => Ok(None),
                Some(having) => Ok(Some(this.bind_plain(having)?)),
            })?;
            self.with_scope(output_scope.clone(), |this| {
                this.bind_projection(&select.projection, &mut items)
            })?;
            for item in order_by {
                let slot = self.with_scope(output_scope.clone(), |this| {
                    this.resolve_output_slot(&item.expr, &mut items, true)
                })?;
                sort_keys.push(SortKey {
                    expr: ScalarExpr::Column(slot),
                    asc: item.asc,
                    nulls_first: item.nulls_first.unwrap_or(!item.asc),
                });
            }
            if let Some(Distinct::OnExprs(exprs)) = &select.distinct {
                for expr in exprs {
                    let slot = self.with_scope(output_scope.clone(), |this| {
                        this.resolve_output_slot(expr, &mut items, false)
                    })?;
                    distinct_slots.push(slot);
                }
            }

            let aggregates = std::mem::take(&mut self.block.aggregates);
            let aggregate_fields = std::mem::take(&mut self.block.aggregate_fields);
            self.block = saved;

            let mut schema = group_fields;
            schema.extend(aggregate_fields);
            plan = Plan::Aggregate {
                input: Box::new(plan),
                groups: group_keys,
                aggregates,
                schema,
            };
            if let Some(predicate) = having {
                plan = Plan::Filter {
                    input: Box::new(plan),
                    predicate,
                };
            }
        } else {
            self.with_scope(from_scope.clone(), |this| {
                this.bind_projection(&select.projection, &mut items)
            })?;
            for item in order_by {
                let slot = self.with_scope(from_scope.clone(), |this| {
                    this.resolve_output_slot(&item.expr, &mut items, true)
                })?;
                sort_keys.push(SortKey {
                    expr: ScalarExpr::Column(slot),
                    asc: item.asc,
                    nulls_first: item.nulls_first.unwrap_or(!item.asc),
                });
            }
            if let Some(Distinct::OnExprs(exprs)) = &select.distinct {
                for expr in exprs {
                    let slot = self.with_scope(from_scope.clone(), |this| {
                        this.resolve_output_slot(expr, &mut items, false)
                    })?;
                    distinct_slots.push(slot);
                }
            }
        }

        // ------------------------------------------------------ projection
        let visible = items.iter().filter(|item| !item.hidden).count();
        let has_hidden = visible != items.len();
        plan = Plan::Project {
            input: Box::new(plan),
            exprs: items.iter().map(|item| item.expr.clone()).collect(),
            schema: items.iter().map(|item| item.field.clone()).collect(),
        };

        // --------------------------------------------------------- ORDER BY
        if !sort_keys.is_empty() {
            plan = Plan::Sort {
                input: Box::new(plan),
                keys: sort_keys,
            };
        }

        // --------------------------------------------------------- DISTINCT
        if let Some(distinct) = &select.distinct {
            let on: Vec<ScalarExpr> = match distinct {
                Distinct::On => {
                    if has_hidden {
                        // PostgreSQL refuses this for the same reason: a hidden
                        // sort key would change the meaning of "duplicate".
                        return Err(ExecError::DistinctOrderBy);
                    }
                    (0..visible).map(ScalarExpr::Column).collect()
                }
                Distinct::OnExprs(_) => distinct_slots
                    .iter()
                    .map(|slot| ScalarExpr::Column(*slot))
                    .collect(),
            };
            plan = Plan::Distinct {
                input: Box::new(plan),
                on,
            };
            if has_hidden {
                // Strip the key-only columns from the result.
                let schema: Schema = items
                    .iter()
                    .filter(|item| !item.hidden)
                    .map(|item| item.field.clone())
                    .collect();
                plan = Plan::Project {
                    input: Box::new(plan),
                    exprs: (0..visible).map(ScalarExpr::Column).collect(),
                    schema,
                };
            }
        }

        // Columns added purely to serve an `ORDER BY` key must not appear in the
        // result. `DISTINCT` handles its own stripping above, so only the plain
        // case is left here.
        if has_hidden && select.distinct.is_none() {
            let schema: Schema = items
                .iter()
                .filter(|item| !item.hidden)
                .map(|item| item.field.clone())
                .collect();
            plan = Plan::Project {
                input: Box::new(plan),
                exprs: (0..visible).map(ScalarExpr::Column).collect(),
                schema,
            };
        }

        self.plan_limit(plan, limit, offset)
    }

    /// Binds a select list, appending to `items`.
    fn bind_projection(
        &mut self,
        projection: &[SelectItem],
        items: &mut Vec<ProjectionItem>,
    ) -> Result<(), ExecError> {
        for item in projection {
            match item {
                SelectItem::Wildcard => {
                    let scope = self.scopes.first().cloned().unwrap_or_default();
                    for (index, column) in scope.columns.iter().enumerate() {
                        if column.hidden {
                            continue;
                        }
                        items.push(ProjectionItem {
                            ast: None,
                            expr: ScalarExpr::Column(index),
                            field: Field {
                                qualifier: column.qualifier.clone(),
                                name: column.name.clone(),
                                data_type: column.data_type.clone(),
                                hidden: false,
                            },
                            hidden: false,
                        });
                    }
                }
                SelectItem::QualifiedWildcard(qualifier) => {
                    let scope = self.scopes.first().cloned().unwrap_or_default();
                    let mut matched = 0;
                    for (index, column) in scope.columns.iter().enumerate() {
                        if column.hidden {
                            continue;
                        }
                        let applies = column
                            .qualifier
                            .as_deref()
                            .map(|candidate| candidate.eq_ignore_ascii_case(qualifier))
                            .unwrap_or(false);
                        if !applies {
                            continue;
                        }
                        matched += 1;
                        items.push(ProjectionItem {
                            ast: None,
                            expr: ScalarExpr::Column(index),
                            field: Field {
                                qualifier: column.qualifier.clone(),
                                name: column.name.clone(),
                                data_type: column.data_type.clone(),
                                hidden: false,
                            },
                            hidden: false,
                        });
                    }
                    if matched == 0 {
                        return Err(ExecError::MissingFromEntry(qualifier.clone()));
                    }
                }
                SelectItem::Expr { expr, alias } => {
                    let bound = self.bind_plain(expr)?;
                    let input_schema = self.scopes.first().cloned().unwrap_or_default().schema();
                    let data_type = bound
                        .infer_type(&input_schema)
                        .or_else(|| constant_type_of(expr))
                        .unwrap_or(DataType::Text);
                    items.push(ProjectionItem {
                        ast: Some(expr.clone()),
                        expr: bound,
                        field: Field::new(None, output_name(expr, alias.as_deref()), data_type),
                        hidden: false,
                    });
                }
            }
        }
        Ok(())
    }

    /// Resolves an `ORDER BY` / `DISTINCT ON` key to a projected slot.
    ///
    /// Resolution follows PostgreSQL's order: an ordinal, then an output name,
    /// then an expression already selected, then any expression over the input
    /// (appended as a hidden column).
    fn resolve_output_slot(
        &mut self,
        expr: &Expr,
        items: &mut Vec<ProjectionItem>,
        positional: bool,
    ) -> Result<usize, ExecError> {
        if positional {
            if let Expr::Literal(Value::Int4(position)) = expr {
                let index = (*position - 1) as usize;
                if *position < 1 || index >= items.len() {
                    return Err(ExecError::other(format!(
                        "ORDER BY position {position} is not in select list"
                    )));
                }
                return Ok(index);
            }
        }
        if let Expr::Column(reference) = expr {
            if reference.qualifier.is_none() {
                let matches: Vec<usize> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.field.name.eq_ignore_ascii_case(&reference.name))
                    .map(|(index, _)| index)
                    .collect();
                match matches.as_slice() {
                    [index] => return Ok(*index),
                    [] => {}
                    _ => return Err(ExecError::AmbiguousColumn(reference.name.clone())),
                }
            }
        }
        for (index, item) in items.iter().enumerate() {
            if item.ast.as_ref() == Some(expr) {
                return Ok(index);
            }
        }
        let bound = self.bind_plain(expr)?;
        for (index, item) in items.iter().enumerate() {
            if item.expr == bound {
                return Ok(index);
            }
        }
        let data_type = bound
            .infer_type(&self.scopes.first().cloned().unwrap_or_default().schema())
            .or_else(|| constant_type_of(expr))
            .unwrap_or(DataType::Text);
        items.push(ProjectionItem {
            ast: Some(expr.clone()),
            expr: bound,
            field: Field::unqualified(expr_name(expr), data_type),
            hidden: true,
        });
        Ok(items.len() - 1)
    }

    // ------------------------------------------------------------ LIMIT

    fn plan_limit(
        &mut self,
        plan: Plan,
        limit: Option<&Expr>,
        offset: Option<&Expr>,
    ) -> Result<Plan, ExecError> {
        if limit.is_none() && offset.is_none() {
            return Ok(plan);
        }
        let limit = match limit {
            None => None,
            Some(expr) => Some(self.const_count(expr)?),
        };
        let offset = match offset {
            None => 0,
            Some(expr) => self.const_count(expr)?,
        };
        Ok(Plan::Limit {
            input: Box::new(plan),
            limit,
            offset,
        })
    }

    fn const_count(&mut self, expr: &Expr) -> Result<u64, ExecError> {
        let bound = self.bind_plain(expr)?;
        let value = bound.eval_constant()?.ok_or(ExecError::InvalidLimit)?;
        match value {
            Value::Null => Err(ExecError::other("LIMIT must not be null")),
            other => {
                let integer = other
                    .as_i64()
                    .ok_or_else(|| ExecError::other("argument of LIMIT must be an integer"))?;
                if integer < 0 {
                    return Err(ExecError::other("LIMIT must not be negative"));
                }
                Ok(integer as u64)
            }
        }
    }

    // -------------------------------------------------------------- FROM

    fn plan_from(&mut self, from: &[TableWithJoins]) -> Result<(Plan, Scope), ExecError> {
        if from.is_empty() {
            return Ok((Plan::SingleRow { schema: Vec::new() }, Scope::default()));
        }
        let mut iter = from.iter();
        let first = iter.next().expect("checked non-empty");
        let (mut plan, mut scope) = self.plan_table_with_joins(first)?;
        for item in iter {
            let (right_plan, right_scope) = self.plan_table_with_joins(item)?;
            let combined = scope.concat(&right_scope);
            let schema = combined.schema();
            plan = Plan::Join {
                left: Box::new(plan),
                right: Box::new(right_plan),
                join_type: JoinType::Cross,
                on: None,
                schema,
            };
            scope = combined;
        }
        Ok((plan, scope))
    }

    fn plan_table_with_joins(&mut self, item: &TableWithJoins) -> Result<(Plan, Scope), ExecError> {
        let (mut plan, mut scope) = self.plan_table_factor(&item.relation)?;
        for join in &item.joins {
            let (right_plan, right_scope) = self.plan_table_factor(&join.relation)?;
            let mut combined = scope.concat(&right_scope);
            let mut using_names: Vec<String> = Vec::new();

            let on = match &join.constraint {
                JoinConstraint::None => None,
                JoinConstraint::On(expr) => {
                    let combined_for_on = combined.clone();
                    Some(self.with_scope(combined_for_on, |this| this.bind_plain(expr))?)
                }
                JoinConstraint::Using(columns) => {
                    let mut conditions = Vec::new();
                    for name in columns {
                        let left_index = scope
                            .lookup(None, name)?
                            .ok_or_else(|| ExecError::UndefinedColumn(name.clone()))?;
                        let right_index = right_scope
                            .lookup(None, name)?
                            .ok_or_else(|| ExecError::UndefinedColumn(name.clone()))?
                            + scope.len();
                        conditions.push(ScalarExpr::Binary {
                            op: vsql_parser::BinaryOp::Eq,
                            left: Box::new(ScalarExpr::Column(left_index)),
                            right: Box::new(ScalarExpr::Column(right_index)),
                        });
                    }
                    using_names = columns.clone();
                    combine(conditions)
                }
                JoinConstraint::Natural => {
                    using_names = scope
                        .columns
                        .iter()
                        .filter(|column| {
                            right_scope
                                .lookup(None, &column.name)
                                .ok()
                                .flatten()
                                .is_some()
                        })
                        .map(|column| column.name.clone())
                        .collect();
                    let mut conditions = Vec::new();
                    for name in &using_names {
                        let left_index = scope.lookup(None, name)?.expect("filtered above");
                        let right_index =
                            right_scope.lookup(None, name)?.expect("filtered above") + scope.len();
                        conditions.push(ScalarExpr::Binary {
                            op: vsql_parser::BinaryOp::Eq,
                            left: Box::new(ScalarExpr::Column(left_index)),
                            right: Box::new(ScalarExpr::Column(right_index)),
                        });
                    }
                    combine(conditions)
                }
            };

            if join.op == JoinType::Cross && on.is_some() {
                return Err(ExecError::other("CROSS JOIN cannot have a join condition"));
            }

            // `USING`/`NATURAL` output their key once: the right-hand copy stays
            // addressable but disappears from `SELECT *`.
            for name in &using_names {
                if let Some(index) = right_scope.lookup(None, name)? {
                    let shifted = index + scope.len();
                    if let Some(column) = combined.columns.get_mut(shifted) {
                        column.hidden = true;
                    }
                }
            }

            let schema = combined.schema();
            plan = Plan::Join {
                left: Box::new(plan),
                right: Box::new(right_plan),
                join_type: join.op,
                on,
                schema,
            };
            scope = combined;
        }
        Ok((plan, scope))
    }

    /// Plans a set-returning function in `FROM`.
    ///
    /// Only the functions the engine answers are implemented; any other name
    /// reports PostgreSQL's `function f(...) does not exist`. The arguments are
    /// evaluated once per query, so a reference to a column - what `LATERAL`
    /// needs - is rejected instead of being read as a null row.
    fn plan_table_function(
        &mut self,
        name: &ast::Name,
        args: &[Expr],
        alias: Option<&ast::TableAlias>,
    ) -> Result<(Plan, Scope), ExecError> {
        let written = name.join(".");
        // `pg_catalog.generate_series(1, 3)` is the same function as the
        // unqualified call, so dispatch on the bare name.
        let bare = written
            .rsplit('.')
            .next()
            .unwrap_or(&written)
            .to_ascii_lowercase();
        let mut arguments = Vec::with_capacity(args.len());
        for arg in args {
            let bound = self.bind_inner(arg)?;
            let mut columns = Vec::new();
            bound.collect_columns(&mut columns);
            if !columns.is_empty() {
                return Err(ExecError::not_supported(format!(
                    "LATERAL call of `{written}`"
                )));
            }
            arguments.push(bound);
        }
        let (function, schema) = match (bare.as_str(), arguments.len()) {
            ("generate_series", 2) => (
                TableFunction::GenerateSeries {
                    start: arguments[0].clone(),
                    stop: arguments[1].clone(),
                    step: ScalarExpr::Const(Value::Int8(1)),
                },
                vec![Field::unqualified("generate_series", DataType::Int8)],
            ),
            ("generate_series", 3) => (
                TableFunction::GenerateSeries {
                    start: arguments[0].clone(),
                    stop: arguments[1].clone(),
                    step: arguments[2].clone(),
                },
                vec![Field::unqualified("generate_series", DataType::Int8)],
            ),
            ("pg_get_keywords", 0) => (
                TableFunction::Keywords,
                vec![
                    Field::unqualified("word", DataType::Text),
                    Field::unqualified("catcode", DataType::Text),
                    Field::unqualified("catdesc", DataType::Text),
                    Field::unqualified("barelabel", DataType::Bool),
                ],
            ),
            ("pg_available_extensions", 0) => (
                TableFunction::AvailableExtensions,
                vec![
                    Field::unqualified("name", DataType::Text),
                    Field::unqualified("default_version", DataType::Text),
                    Field::unqualified("installed_version", DataType::Text),
                    Field::unqualified("comment", DataType::Text),
                ],
            ),
            ("pg_available_extension_versions", 0) => (
                TableFunction::AvailableExtensionVersions,
                vec![
                    Field::unqualified("name", DataType::Text),
                    Field::unqualified("version", DataType::Text),
                    Field::unqualified("installed", DataType::Bool),
                    Field::unqualified("superuser", DataType::Bool),
                    Field::unqualified("trusted", DataType::Bool),
                    Field::unqualified("relocatable", DataType::Bool),
                    Field::unqualified("schema", DataType::Text),
                    Field::unqualified("requires", DataType::Array(Box::new(DataType::Text))),
                    Field::unqualified("comment", DataType::Text),
                ],
            ),
            ("unnest", 1) => {
                let element = match arguments[0].infer_type(&[]) {
                    Some(DataType::Array(element)) => *element,
                    _ => DataType::Text,
                };
                (
                    TableFunction::Unnest {
                        array: arguments[0].clone(),
                    },
                    vec![Field::unqualified("unnest", element)],
                )
            }
            // PostgreSQL names the function together with the types it was
            // called with, which is what makes the message actionable.
            _ => {
                let types: Vec<String> = arguments
                    .iter()
                    .map(|arg| match arg.infer_type(&[]) {
                        Some(data_type) => data_type.to_string(),
                        None => "unknown".to_string(),
                    })
                    .collect();
                return Err(ExecError::UndefinedFunction(format!(
                    "{written}({})",
                    types.join(", ")
                )));
            }
        };
        let schema = alias_schema(&schema, alias, &bare);
        let scope = Scope::from_schema(&schema);
        Ok((
            Plan::TableFunction(Box::new(TableFunctionPlan { function, schema })),
            scope,
        ))
    }

    fn plan_table_factor(&mut self, factor: &TableFactor) -> Result<(Plan, Scope), ExecError> {
        match factor {
            TableFactor::Table { name, alias } => self.plan_named_relation(name, alias.as_ref()),
            TableFactor::Derived {
                subquery,
                alias,
                lateral,
            } => {
                if *lateral {
                    return Err(ExecError::not_supported("LATERAL subqueries"));
                }
                let Some(alias) = alias else {
                    return Err(ExecError::other("subquery in FROM must have an alias"));
                };
                // A `FROM` subquery is its own block: it may not see the columns
                // of the query that contains it (that is what `LATERAL` is for).
                let plan = self.plan_detached_query(subquery)?;
                let schema = alias_schema(plan.schema(), Some(alias), &alias.name);
                let scope = Scope::from_schema(&schema);
                Ok((
                    Plan::Alias {
                        input: Box::new(plan),
                        schema,
                    },
                    scope,
                ))
            }
            TableFactor::Function { name, args, alias } => {
                self.plan_table_function(name, args, alias.as_ref())
            }
            TableFactor::NestedJoin { table, alias } => {
                let (plan, scope) = self.plan_table_with_joins(table)?;
                match alias {
                    None => Ok((plan, scope)),
                    Some(alias) => {
                        let schema = alias_schema(plan.schema(), Some(alias), &alias.name);
                        let scope = Scope::from_schema(&schema);
                        Ok((
                            Plan::Alias {
                                input: Box::new(plan),
                                schema,
                            },
                            scope,
                        ))
                    }
                }
            }
        }
    }

    /// Resolves a name to a CTE, a view or a base table.
    fn plan_named_relation(
        &mut self,
        name: &[String],
        alias: Option<&ast::TableAlias>,
    ) -> Result<(Plan, Scope), ExecError> {
        let relation = name
            .last()
            .cloned()
            .ok_or_else(|| ExecError::other("empty relation name"))?;

        // CTEs shadow tables of the same name.
        let cte = self
            .ctes
            .iter()
            .rev()
            .find(|(cte, _)| cte.eq_ignore_ascii_case(&relation))
            .map(|(_, query)| query.clone());
        if let Some(query) = cte {
            let plan = self.plan_detached_query(&query)?;
            let schema = alias_schema(plan.schema(), alias, &relation);
            let scope = Scope::from_schema(&schema);
            return Ok((
                Plan::Alias {
                    input: Box::new(plan),
                    schema,
                },
                scope,
            ));
        }

        if let Some(query) = self.database.view(name) {
            let plan = self.plan_detached_query(&query)?;
            let schema = alias_schema(plan.schema(), alias, &relation);
            let scope = Scope::from_schema(&schema);
            return Ok((
                Plan::Alias {
                    input: Box::new(plan),
                    schema,
                },
                scope,
            ));
        }

        // System catalogue relations come before user tables, exactly as
        // `pg_catalog` precedes everything else in PostgreSQL's search path.
        if let Some(source) = crate::catalog_scan::CatalogSource::resolve(name) {
            let schema = alias_schema(&source.schema(), alias, &relation);
            let scope = Scope::from_schema(&schema);
            return Ok((
                Plan::CatalogScan(Box::new(crate::plan::CatalogScanPlan { source, schema })),
                scope,
            ));
        }

        let def = match self.database.catalog.resolve_table(name) {
            Ok(def) => def,
            Err(error) => {
                // Clients reach for `information_schema.parameters` by its bare
                // name, the way they would with `information_schema` on their
                // search path. User relations were already tried and missed, so
                // a well-known information_schema view cannot shadow one here.
                if name.len() == 1 {
                    if let Some(source) = crate::catalog_scan::CatalogSource::resolve(&[
                        "information_schema".to_string(),
                        relation.clone(),
                    ]) {
                        let schema = alias_schema(&source.schema(), alias, &relation);
                        let scope = Scope::from_schema(&schema);
                        return Ok((
                            Plan::CatalogScan(Box::new(crate::plan::CatalogScanPlan {
                                source,
                                schema,
                            })),
                            scope,
                        ));
                    }
                }
                return Err(error.into());
            }
        };
        let Some(table) = self.database.table(def.oid) else {
            return Err(ExecError::Catalog(
                vsql_catalog::CatalogError::UndefinedTable(def.name.clone()),
            ));
        };
        let qualifier = alias
            .map(|alias| alias.name.clone())
            .unwrap_or_else(|| def.name.clone());
        let columns: Vec<usize> = def
            .columns
            .iter()
            .filter(|column| !column.dropped)
            .map(|column| column.ordinal)
            .collect();
        let mut schema: Schema = columns
            .iter()
            .map(|ordinal| {
                let column = &def.columns[*ordinal];
                Field::new(
                    Some(qualifier.clone()),
                    column.name.clone(),
                    column.data_type.clone(),
                )
            })
            .collect();
        if let Some(alias) = alias {
            if !alias.columns.is_empty() {
                schema = alias_schema(&schema, Some(alias), &qualifier);
            }
        }
        let scope = Scope::from_schema(&schema);
        Ok((
            Plan::Scan(Box::new(ScanPlan {
                table,
                columns,
                schema,
                index: None,
                filter: None,
            })),
            scope,
        ))
    }

    // ------------------------------------------------------------ filtering

    /// Applies `predicate`, pushing conjuncts into a base scan when possible.
    fn apply_filter(
        &mut self,
        plan: Plan,
        predicate: ScalarExpr,
        scope: &Scope,
    ) -> Result<Plan, ExecError> {
        let mut conjuncts = Vec::new();
        split_conjuncts(predicate, &mut conjuncts);

        if let Plan::Scan(scan) = &plan {
            let width = scope.len();
            let mut pushable = Vec::new();
            let mut residual = Vec::new();
            for conjunct in conjuncts {
                let mut columns = Vec::new();
                conjunct.collect_columns(&mut columns);
                if columns.iter().all(|index| *index < width) {
                    pushable.push(conjunct);
                } else {
                    residual.push(conjunct);
                }
            }
            let mut scan = (**scan).clone();
            scan.index = self.choose_index(&scan, &pushable);
            scan.filter = combine(pushable);
            let plan = Plan::Scan(Box::new(scan));
            return Ok(match combine(residual) {
                None => plan,
                Some(predicate) => Plan::Filter {
                    input: Box::new(plan),
                    predicate,
                },
            });
        }

        Ok(match combine(conjuncts) {
            None => plan,
            Some(predicate) => Plan::Filter {
                input: Box::new(plan),
                predicate,
            },
        })
    }

    /// Picks an index for an equality predicate on the index's leading column.
    fn choose_index(&self, scan: &ScanPlan, conjuncts: &[ScalarExpr]) -> Option<IndexScanPlan> {
        for conjunct in conjuncts {
            let ScalarExpr::Binary {
                op: vsql_parser::BinaryOp::Eq,
                left,
                right,
            } = conjunct
            else {
                continue;
            };
            for (column_expr, key_expr) in [(left, right), (right, left)] {
                let ScalarExpr::Column(slot) = column_expr.as_ref() else {
                    continue;
                };
                if !key_expr.is_constant() {
                    continue;
                }
                let Some(ordinal) = scan.columns.get(*slot) else {
                    continue;
                };
                let index = self
                    .database
                    .catalog
                    .table_indexes(scan.table.def().oid)
                    .into_iter()
                    .find(|index| index.columns.first() == Some(ordinal))?;
                return Some(IndexScanPlan {
                    index,
                    keys: vec![key_expr.as_ref().clone()],
                });
            }
        }
        None
    }

    // --------------------------------------------------------------- binding

    fn with_scope<R>(
        &mut self,
        scope: Scope,
        f: impl FnOnce(&mut Self) -> Result<R, ExecError>,
    ) -> Result<R, ExecError> {
        self.scopes.insert(0, scope);
        let result = f(self);
        self.scopes.remove(0);
        result
    }

    /// Binds an expression in an empty scope.
    ///
    /// This is the entry point for `INSERT ... VALUES`, whose expressions may
    /// not reference any column.
    pub fn bind_expression(&mut self, expr: &Expr) -> Result<ScalarExpr, ExecError> {
        self.bind_inner(expr)
    }

    /// Binds an expression against a synthetic relation schema.
    ///
    /// Used by `UPDATE`/`DELETE`/`RETURNING`, which address the target relation
    /// by its own column names rather than through a `FROM` clause.
    pub fn bind_expression_with_schema(
        &mut self,
        expr: &Expr,
        schema: &[Field],
    ) -> Result<ScalarExpr, ExecError> {
        let scope = Scope::from_schema(schema);
        self.with_scope(scope, |this| this.bind_inner(expr))
    }

    /// Binds an expression that may not contain aggregates.
    fn bind_plain(&mut self, expr: &Expr) -> Result<ScalarExpr, ExecError> {
        self.bind_inner(expr)
    }

    fn bind_subquery(&mut self, query: &Query) -> Result<SubPlan, ExecError> {
        let plan = self.plan_query(query)?;
        Ok(SubPlan(Arc::new(plan)))
    }

    fn bind_inner(&mut self, expr: &Expr) -> Result<ScalarExpr, ExecError> {
        // In a grouped block, an expression that *is* a group key refers to the
        // key's output slot. This is what makes `SELECT a + 1 ... GROUP BY a + 1`
        // legal, and it is checked before any name resolution.
        if self.block.aggregate_mode {
            if let Some(index) = self.block.group_key_asts.iter().position(|key| key == expr) {
                return Ok(ScalarExpr::Column(index));
            }
        }

        let bound = match expr {
            Expr::Literal(value) => ScalarExpr::Const(value.clone()),
            Expr::Parameter(index) => ScalarExpr::Parameter(*index),
            Expr::Column(reference) => self.resolve_column(reference)?,
            Expr::Nested(inner) => self.bind_inner(inner)?,
            Expr::Unary { op, expr } => ScalarExpr::Unary {
                op: *op,
                expr: Box::new(self.bind_inner(expr)?),
            },
            Expr::Binary { left, op, right } => {
                let left = self.bind_inner(left)?;
                let right = self.bind_inner(right)?;
                let schema = self.scopes.first().cloned().unwrap_or_default().schema();
                coerce_literals(*op, left, right, &schema)
            }
            Expr::IsNull { expr, negated } => ScalarExpr::IsNull {
                expr: Box::new(self.bind_inner(expr)?),
                negated: *negated,
            },
            Expr::IsBool {
                expr,
                value,
                negated,
            } => ScalarExpr::IsBool {
                expr: Box::new(self.bind_inner(expr)?),
                value: *value,
                negated: *negated,
            },
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let expr = self.bind_inner(expr)?;
                let mut bound = Vec::with_capacity(list.len());
                for item in list {
                    bound.push(self.bind_inner(item)?);
                }
                ScalarExpr::InList {
                    expr: Box::new(expr),
                    list: bound,
                    negated: *negated,
                }
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let expr = self.bind_inner(expr)?;
                let plan = self.bind_subquery(subquery)?;
                ScalarExpr::InSubquery {
                    expr: Box::new(expr),
                    plan,
                    negated: *negated,
                }
            }
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => ScalarExpr::Between {
                expr: Box::new(self.bind_inner(expr)?),
                low: Box::new(self.bind_inner(low)?),
                high: Box::new(self.bind_inner(high)?),
                negated: *negated,
            },
            Expr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
                escape,
            } => {
                if escape.is_some() {
                    return Err(ExecError::not_supported("LIKE ... ESCAPE"));
                }
                ScalarExpr::Like {
                    expr: Box::new(self.bind_inner(expr)?),
                    pattern: Box::new(self.bind_inner(pattern)?),
                    negated: *negated,
                    case_insensitive: *case_insensitive,
                }
            }
            Expr::Cast {
                expr,
                data_type,
                try_cast,
            } => ScalarExpr::Cast {
                expr: Box::new(self.bind_inner(expr)?),
                data_type: data_type.clone(),
                try_cast: *try_cast,
            },
            Expr::Case {
                operand,
                whens,
                else_result,
            } => {
                let operand = match operand {
                    Some(operand) => Some(Box::new(self.bind_inner(operand)?)),
                    None => None,
                };
                let mut bound = Vec::with_capacity(whens.len());
                for (condition, result) in whens {
                    bound.push((self.bind_inner(condition)?, self.bind_inner(result)?));
                }
                let else_result = match else_result {
                    Some(result) => Some(Box::new(self.bind_inner(result)?)),
                    None => None,
                };
                ScalarExpr::Case {
                    operand,
                    whens: bound,
                    else_result,
                }
            }
            Expr::Function(call) => {
                if let Some(func) = aggregate_func(&call.name) {
                    // Every aggregate the engine implements folds its inputs
                    // with a commutative operation, so an `ORDER BY` written
                    // inside the call cannot change the result: it is
                    // accepted and dropped here.
                    return self.bind_aggregate(call, func);
                }
                if !call.order_by.is_empty() {
                    // PostgreSQL words it exactly this way.
                    return Err(ExecError::other(format!(
                        "ORDER BY specified, but {} is not an aggregate function",
                        call.name
                    )));
                }
                let name = function_name(&call.name)?;
                let mut args = Vec::with_capacity(call.args.len());
                for arg in &call.args {
                    args.push(self.bind_inner(arg)?);
                }
                if call.star {
                    return Err(ExecError::UndefinedFunction(format!("{}(*)", name)));
                }
                ScalarExpr::Function { name, args }
            }
            Expr::Subquery(query) => ScalarExpr::ScalarSubquery(self.bind_subquery(query)?),
            Expr::Exists { subquery, negated } => ScalarExpr::Exists {
                plan: self.bind_subquery(subquery)?,
                negated: *negated,
            },
            Expr::AnyAll {
                left,
                op,
                right,
                is_all,
            } => {
                let left = self.bind_inner(left)?;
                // PostgreSQL turns `= ANY (subquery)` into `IN (subquery)` and
                // `<> ALL (subquery)` into `NOT IN (subquery)`; those two reuse
                // the `IN` machinery, everything else compares candidate by
                // candidate.
                match (&**right, *op, *is_all) {
                    (Expr::Subquery(query), BinaryOp::Eq, false)
                    | (Expr::Subquery(query), BinaryOp::NotEq, true) => {
                        let plan = self.bind_subquery(query)?;
                        ScalarExpr::InSubquery {
                            expr: Box::new(left),
                            plan,
                            negated: *is_all,
                        }
                    }
                    (Expr::Subquery(query), _, is_all) => {
                        let plan = self.bind_subquery(query)?;
                        ScalarExpr::AnyAll {
                            left: Box::new(left),
                            op: *op,
                            source: AnyAllSource::Subquery(plan),
                            is_all,
                        }
                    }
                    (array, _, is_all) => ScalarExpr::AnyAll {
                        left: Box::new(left),
                        op: *op,
                        source: AnyAllSource::Array(Box::new(self.bind_inner(array)?)),
                        is_all,
                    },
                }
            }
            Expr::Array(items) => {
                let mut bound = Vec::with_capacity(items.len());
                for item in items {
                    bound.push(self.bind_inner(item)?);
                }
                ScalarExpr::Array(bound)
            }
            Expr::Row(items) => {
                let mut bound = Vec::with_capacity(items.len());
                for item in items {
                    bound.push(self.bind_inner(item)?);
                }
                ScalarExpr::Row(bound)
            }
            Expr::Subscript { expr, index } => ScalarExpr::Subscript {
                expr: Box::new(self.bind_inner(expr)?),
                index: Box::new(self.bind_inner(index)?),
            },
            Expr::Wildcard | Expr::QualifiedWildcard(_) => {
                return Err(ExecError::other("`*` is not valid in this position"));
            }
        };
        Ok(bound)
    }

    fn resolve_column(&mut self, reference: &ast::ColumnRef) -> Result<ScalarExpr, ExecError> {
        let qualifier = reference.qualifier.as_deref();
        for (level, scope) in self.scopes.iter().enumerate() {
            if let Some(index) = scope.lookup(qualifier, &reference.name)? {
                return Ok(if level == 0 {
                    ScalarExpr::Column(index)
                } else {
                    ScalarExpr::Outer {
                        levels_up: level,
                        index,
                    }
                });
            }
        }
        // An unqualified quoted name that matches no column is the string it
        // spells: clients written for MySQL quote their literals that way
        // (`WHERE type = "allowRegister"`), and PostgreSQL would only ever read
        // it as a column and fail. Unquoted names keep PostgreSQL's behaviour,
        // so a misspelled column is still an error rather than a literal, and a
        // qualified one is always a reference (`"settings"."type"`).
        if qualifier.is_none() && reference.quoted {
            return Ok(ScalarExpr::Const(Value::Text(reference.name.clone())));
        }
        if self.block.aggregate_mode {
            return Err(ExecError::UngroupedColumn(reference.name.clone()));
        }
        match qualifier {
            Some(qualifier) => {
                let known = self.scopes.iter().any(|scope| {
                    scope.columns.iter().any(|column| {
                        !column.hidden
                            && column
                                .qualifier
                                .as_deref()
                                .map(|candidate| candidate.eq_ignore_ascii_case(qualifier))
                                .unwrap_or(false)
                    })
                });
                if known {
                    // PostgreSQL names the offending column in full.
                    Err(ExecError::UndefinedColumn(format!(
                        "{qualifier}.{}",
                        reference.name
                    )))
                } else {
                    Err(ExecError::MissingFromEntry(qualifier.to_string()))
                }
            }
            None => Err(ExecError::UndefinedColumn(reference.name.clone())),
        }
    }

    /// Binds an aggregate call, deduplicating structurally identical calls.
    fn bind_aggregate(
        &mut self,
        call: &ast::FunctionCall,
        func: AggregateFunc,
    ) -> Result<ScalarExpr, ExecError> {
        let Some(input_scope) = self.block.aggregate_input_scope.clone() else {
            return Err(ExecError::AggregateNotAllowed(
                "a non-aggregate context".to_string(),
            ));
        };
        // An aggregate's argument is bound against the *input* row, with
        // grouping disabled: `count(a)` must not be rewritten to a group slot.
        let saved = std::mem::replace(
            &mut self.block,
            BlockState {
                aggregate_mode: false,
                aggregate_input_scope: Some(input_scope.clone()),
                group_keys: Vec::new(),
                group_key_asts: Vec::new(),
                aggregates: Vec::new(),
                aggregate_fields: Vec::new(),
            },
        );
        let argument = self.with_scope(input_scope.clone(), |this| match call.args.first() {
            None => Ok(None),
            Some(arg) => Ok(Some(this.bind_inner(arg)?)),
        });
        // `string_agg(value, delimiter)` binds its second argument the same
        // way; other aggregates ignore extra arguments, as before. An `ORDER
        // BY` inside the call is accepted but folded away: rows reach the
        // accumulator in input order, which keeps metadata queries that spell
        // `string_agg(name, ', ' ORDER BY pos)` working.
        let delimiter = self.with_scope(input_scope.clone(), |this| match call.args.get(1) {
            None => Ok(None),
            Some(arg) => Ok(Some(this.bind_inner(arg)?)),
        });
        let filter = self.with_scope(input_scope.clone(), |this| match &call.filter {
            None => Ok(None),
            Some(filter) => Ok(Some(this.bind_inner(filter)?)),
        });
        self.block = saved;
        let argument = argument?;
        let delimiter = delimiter?;
        let filter = filter?;

        // `count(*)` counts rows rather than values, so it gets its own
        // accumulator; every other aggregate returns `NULL` for a `NULL` input.
        let func = if call.star && func == AggregateFunc::Count {
            AggregateFunc::CountStar
        } else {
            func
        };
        let candidate = AggregateCall {
            func,
            arg: if call.star { None } else { argument },
            delimiter: if func == AggregateFunc::StringAgg {
                delimiter
            } else {
                None
            },
            distinct: call.distinct,
            filter,
        };
        let position = match self
            .block
            .aggregates
            .iter()
            .position(|existing| *existing == candidate)
        {
            Some(position) => position,
            None => {
                let argument_type = candidate
                    .arg
                    .as_ref()
                    .and_then(|arg| arg.infer_type(&input_scope.schema()));
                let data_type = candidate.func.result_type(argument_type.as_ref());
                self.block
                    .aggregate_fields
                    .push(Field::unqualified(call.name.clone(), data_type));
                self.block.aggregates.push(candidate);
                self.block.aggregates.len() - 1
            }
        };
        Ok(ScalarExpr::Aggregate(
            self.block.group_keys.len() + position,
        ))
    }

    /// Plans an `ORDER BY` over a node whose schema is already the output.
    fn plan_output_sort(
        &mut self,
        plan: Plan,
        order_by: &[OrderByExpr],
    ) -> Result<Plan, ExecError> {
        let schema = plan.schema().to_vec();
        let scope = Scope::from_schema(&schema);
        let mut keys = Vec::with_capacity(order_by.len());
        for item in order_by {
            let expr = match &item.expr {
                Expr::Literal(Value::Int4(position)) => {
                    let index = (*position - 1) as usize;
                    if *position < 1 || index >= schema.len() {
                        return Err(ExecError::other(format!(
                            "ORDER BY position {position} is not in select list"
                        )));
                    }
                    ScalarExpr::Column(index)
                }
                Expr::Column(reference) => match scope.lookup(None, &reference.name)? {
                    Some(index) => ScalarExpr::Column(index),
                    // See `resolve_column`: a quoted name with no matching
                    // output column is the string it spells.
                    None if reference.quoted => {
                        ScalarExpr::Const(Value::Text(reference.name.clone()))
                    }
                    None => return Err(ExecError::UndefinedColumn(reference.name.clone())),
                },
                _ => {
                    return Err(ExecError::not_supported(
                        "ORDER BY over a set operation may only use output columns",
                    ))
                }
            };
            keys.push(SortKey {
                expr,
                asc: item.asc,
                nulls_first: item.nulls_first.unwrap_or(!item.asc),
            });
        }
        Ok(Plan::Sort {
            input: Box::new(plan),
            keys,
        })
    }
}

/// Splits an expression on top-level `AND`s.
fn split_conjuncts(expr: ScalarExpr, out: &mut Vec<ScalarExpr>) {
    match expr {
        ScalarExpr::Binary {
            op: vsql_parser::BinaryOp::And,
            left,
            right,
        } => {
            split_conjuncts(*left, out);
            split_conjuncts(*right, out);
        }
        other => out.push(other),
    }
}

/// Recombines conjuncts with `AND`.
fn combine(conjuncts: Vec<ScalarExpr>) -> Option<ScalarExpr> {
    conjuncts
        .into_iter()
        .reduce(|left, right| ScalarExpr::Binary {
            op: vsql_parser::BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        })
}

/// Coerces an untyped string literal against its counterpart's type.
///
/// PostgreSQL resolves `id = '5'` to `id = 5`; doing it at plan time keeps the
/// per-row path free of conversion attempts.
fn coerce_literals(
    op: vsql_parser::BinaryOp,
    left: ScalarExpr,
    right: ScalarExpr,
    schema: &[Field],
) -> ScalarExpr {
    use vsql_parser::BinaryOp;
    let comparison = matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
    );
    if !comparison {
        return ScalarExpr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    let left = coerce_against(left, &right, schema);
    let right = coerce_against(right, &left, schema);
    ScalarExpr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn coerce_against(expr: ScalarExpr, other: &ScalarExpr, schema: &[Field]) -> ScalarExpr {
    let Some(target) = other.infer_type(schema) else {
        return expr;
    };
    // Text against text needs no coercion.
    if matches!(
        target,
        DataType::Text | DataType::Varchar(_) | DataType::Char(_)
    ) {
        return expr;
    }
    match &expr {
        // `id = '5'`: fold a parseable literal so an index probe carries the
        // column's type; keep a runtime cast otherwise, which PostgreSQL reports
        // as "invalid input syntax" rather than a missing operator.
        ScalarExpr::Const(Value::Text(text)) => {
            if accepts_text(&target) {
                match Value::Text(text.clone()).cast_to(&target) {
                    Ok(value) => ScalarExpr::Const(value),
                    Err(_) => ScalarExpr::Cast {
                        expr: Box::new(expr),
                        data_type: target,
                        try_cast: false,
                    },
                }
            } else {
                expr
            }
        }
        // `int_col = text_col`: wrap an implicit cast to the other side's type,
        // the way PostgreSQL reads the textual value as an integer/boolean.
        ScalarExpr::Column(_) => {
            if let Some(DataType::Text | DataType::Varchar(_) | DataType::Char(_)) =
                expr.infer_type(schema)
            {
                if is_numeric(&target) || matches!(target, DataType::Bool) {
                    return ScalarExpr::Cast {
                        expr: Box::new(expr),
                        data_type: target,
                        try_cast: false,
                    };
                }
            }
            expr
        }
        // `big_col = 2`: an integer literal is int4, but the comparison happens
        // in the column's type - and an index probe built from the literal must
        // carry the column's own type, or it never matches the stored keys.
        ScalarExpr::Const(value @ (Value::Int2(_) | Value::Int4(_) | Value::Int8(_)))
            if is_numeric(&target) =>
        {
            match value.clone().cast_to(&target) {
                Ok(coerced) => ScalarExpr::Const(coerced),
                Err(_) => expr,
            }
        }
        // `int_col = $1`: a driver sends bind parameters in text format, so the
        // value arrives as text and has to be read as the counterpart's type —
        // PostgreSQL resolves a parameter's type from its context the same way.
        // The cast runs at execution time because the value is only known then,
        // and it must be planned in: an index probe built from the untyped text
        // would compare against the wrong type and silently match no rows.
        ScalarExpr::Parameter(_) if accepts_text(&target) => ScalarExpr::Cast {
            expr: Box::new(expr),
            data_type: target,
            try_cast: false,
        },
        _ => expr,
    }
}

/// `true` for the types a text value can be read as.
///
/// This is the set PostgreSQL resolves an `unknown` literal (or a parameter
/// whose type it infers from context) against, so `ts_col > '2020-01-01'`
/// compares two timestamps rather than failing with
/// `operator does not exist: timestamp with time zone > text`.
fn accepts_text(data_type: &DataType) -> bool {
    is_numeric(data_type)
        || matches!(
            data_type,
            DataType::Bool
                | DataType::Date
                | DataType::Time
                | DataType::Timestamp
                | DataType::Timestamptz
                | DataType::Interval
                | DataType::Uuid
        )
}

/// `true` for the types whose values compare numerically.
fn is_numeric(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Int2
            | DataType::Int4
            | DataType::Int8
            | DataType::Float4
            | DataType::Float8
            | DataType::Numeric { .. }
    )
}

/// Maps a function name to an aggregate, if it is one.
fn aggregate_func(name: &str) -> Option<AggregateFunc> {
    let name = name.rsplit('.').next().unwrap_or(name);
    let func = match name.to_ascii_lowercase().as_str() {
        "count" => AggregateFunc::Count,
        "sum" => AggregateFunc::Sum,
        "avg" => AggregateFunc::Avg,
        "min" => AggregateFunc::Min,
        "max" => AggregateFunc::Max,
        "string_agg" => AggregateFunc::StringAgg,
        _ => return None,
    };
    Some(func)
}

/// `true` when the expression contains an aggregate call.
///
/// Subqueries are not descended into: an aggregate inside a subquery belongs to
/// that subquery's own block.
fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(call) => {
            aggregate_func(&call.name).is_some() || call.args.iter().any(contains_aggregate)
        }
        Expr::Subquery(_)
        | Expr::Exists { .. }
        | Expr::Literal(_)
        | Expr::Column(_)
        | Expr::Parameter(_)
        | Expr::Wildcard
        | Expr::QualifiedWildcard(_) => false,
        Expr::Nested(inner) => contains_aggregate(inner),
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Binary { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::InSubquery { expr, .. } => contains_aggregate(expr),
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::Case {
            operand,
            whens,
            else_result,
        } => {
            operand
                .as_ref()
                .map(|operand| contains_aggregate(operand))
                .unwrap_or(false)
                || whens.iter().any(|(condition, value)| {
                    contains_aggregate(condition) || contains_aggregate(value)
                })
                || else_result
                    .as_ref()
                    .map(|result| contains_aggregate(result))
                    .unwrap_or(false)
        }
        Expr::AnyAll { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::Array(items) | Expr::Row(items) => items.iter().any(contains_aggregate),
        Expr::Subscript { expr, index } => contains_aggregate(expr) || contains_aggregate(index),
    }
}

/// The output name PostgreSQL gives an expression.
fn output_name(expr: &Expr, alias: Option<&str>) -> String {
    if let Some(alias) = alias {
        return alias.to_string();
    }
    match expr {
        Expr::Column(reference) => reference.name.clone(),
        Expr::Function(call) => call
            .name
            .rsplit('.')
            .next()
            .unwrap_or(&call.name)
            .to_string(),
        Expr::Cast { data_type, .. } => data_type.format_type(),
        Expr::Nested(inner) => output_name(inner, None),
        Expr::Case { .. } => "case".to_string(),
        _ => "?column?".to_string(),
    }
}

/// Resolves a function name, accepting PostgreSQL's `pg_catalog.` qualification.
///
/// Tools qualify system functions (`pg_catalog.pg_get_userbyid(...)`), which
/// selects exactly the same built-ins; any other schema qualifier names a schema
/// that has no user-defined functions to hide behind, so it stays undefined.
fn function_name(name: &str) -> Result<String, ExecError> {
    match name.rsplit_once('.') {
        None => Ok(name.to_ascii_lowercase()),
        Some((schema, function)) => {
            if schema.eq_ignore_ascii_case("pg_catalog") {
                Ok(function.to_ascii_lowercase())
            } else {
                Err(ExecError::UndefinedFunction(name.to_string()))
            }
        }
    }
}

/// A best-effort name for expressions used as keys.
fn expr_name(expr: &Expr) -> String {
    match expr {
        Expr::Column(reference) => reference.name.clone(),
        _ => "?column?".to_string(),
    }
}

/// The type of an expression that needs no row to evaluate.
fn constant_type_of(expr: &Expr) -> Option<DataType> {
    match expr {
        Expr::Literal(value) if !value.is_null() => Some(value.type_of()),
        Expr::Cast { data_type, .. } => Some(data_type.clone()),
        Expr::Nested(inner) => constant_type_of(inner),
        _ => None,
    }
}

/// Re-labels a schema with a relation alias.
///
/// `fallback` is the relation's own name, used when the `FROM` item has no
/// alias: `FROM users` still qualifies its columns as `users.id`.
fn alias_schema(schema: &[Field], alias: Option<&ast::TableAlias>, fallback: &str) -> Schema {
    schema
        .iter()
        .enumerate()
        .map(|(index, field)| Field {
            qualifier: Some(
                alias
                    .map(|alias| alias.name.clone())
                    .unwrap_or_else(|| fallback.to_string()),
            ),
            name: alias
                .and_then(|alias| alias.columns.get(index).cloned())
                .unwrap_or_else(|| field.name.clone()),
            data_type: field.data_type.clone(),
            hidden: field.hidden,
        })
        .collect()
}
