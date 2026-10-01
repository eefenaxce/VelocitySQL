//! `EXPLAIN` output.
//!
//! The formatting follows `psql`'s conventions closely enough that tools which
//! parse `EXPLAIN` keep working:
//!
//! ```text
//! Sort  (cost=0.00..0.00 rows=2 width=8)
//!   Sort Key: a DESC
//!   ->  Seq Scan on t  (cost=0.00..0.00 rows=8 width=8)
//!         Filter: (b > 1)
//! ```
//!
//! VelocitySQL has no cost model yet, so `cost` is reported as `0.00..0.00` and
//! `rows` is a *row-count* estimate derived from the catalog's live row counts
//! and fixed selectivities. `EXPLAIN ANALYZE` additionally reports the real row
//! count of the top node.

use vsql_types::DataType;

use crate::plan::{AggregateFunc, Plan, ScanPlan, SetOperator, TableFunction};
use crate::render;

/// Estimated rows for a node, using PostgreSQL's default selectivities.
///
/// These are placeholders for a real cost model (milestone M5), but they are
/// *stable*: the same query always prints the same numbers, which is what makes
/// them usable in regression tests.
pub fn estimate_rows(plan: &Plan) -> u64 {
    match plan {
        Plan::Scan(scan) => {
            let rows = scan.table.def().rows();
            if scan.index.is_some() {
                // A unique probe returns at most one row.
                rows.min(1)
            } else {
                rows
            }
        }
        Plan::CatalogScan(_) => 100,
        Plan::TableFunction(function) => match &function.function {
            TableFunction::GenerateSeries { .. } => 1000,
            TableFunction::Keywords => vsql_parser::keywords().count() as u64,
            TableFunction::AvailableExtensions | TableFunction::AvailableExtensionVersions => 1,
            TableFunction::Unnest { .. } => 100,
        },
        Plan::SingleRow { .. } => 1,
        Plan::Values { rows, .. } => rows.len() as u64,
        Plan::Filter { input, .. } => (estimate_rows(input) / 3).max(1),
        Plan::Join { left, right, .. } => {
            ((estimate_rows(left) as u128 * estimate_rows(right) as u128 / 10).max(1)) as u64
        }
        Plan::Aggregate {
            input, aggregates, ..
        } => {
            let rows = estimate_rows(input) / 10;
            if aggregates
                .iter()
                .any(|aggregate| aggregate.func == AggregateFunc::CountStar)
                && rows == 0
            {
                1
            } else {
                rows.max(1)
            }
        }
        Plan::Distinct { input, .. } => (estimate_rows(input) / 2).max(1),
        Plan::SetOp {
            left, right, all, ..
        } => {
            let left = estimate_rows(left);
            let right = estimate_rows(right);
            if *all {
                left + right
            } else {
                (left + right) / 2
            }
        }
        Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Project { input, .. }
        | Plan::Alias { input, .. } => estimate_rows(input),
    }
}

/// Approximate tuple width in bytes, the way `EXPLAIN` reports it.
fn schema_width(schema: &[crate::Field]) -> usize {
    schema
        .iter()
        .map(|field| match &field.data_type {
            DataType::Bool => 1,
            DataType::Int2 => 2,
            DataType::Int4 | DataType::Float4 | DataType::Date => 4,
            DataType::Int8 | DataType::Float8 | DataType::Timestamp | DataType::Timestamptz => 8,
            DataType::Time => 8,
            DataType::Interval => 16,
            DataType::Uuid => 16,
            DataType::Numeric { .. } => 16,
            DataType::Text | DataType::Json | DataType::Jsonb => 32,
            DataType::Varchar(_) | DataType::Char(_) | DataType::Bytea => 32,
            DataType::Array(_) => 32,
        })
        .sum()
}

/// Formats a plan the way PostgreSQL's `EXPLAIN` does.
pub fn explain(plan: &Plan, analyze: bool, actual_rows: Option<u64>) -> Vec<String> {
    let mut lines = Vec::new();
    walk(plan, 0, Vec::new(), &mut lines, analyze, actual_rows);
    if analyze {
        lines.push("Planning Time: 0.000 ms".to_string());
        lines.push("Execution Time: 0.000 ms".to_string());
    }
    lines
}

/// Emits the node for `plan`, folding its input's properties in.
fn walk(
    plan: &Plan,
    indent: usize,
    properties: Vec<String>,
    lines: &mut Vec<String>,
    analyze: bool,
    actual_rows: Option<u64>,
) {
    match plan {
        // Filters and projections are properties of their input in PostgreSQL's
        // output, so they are folded in rather than printed as their own nodes.
        Plan::Filter { input, predicate } => {
            let mut properties = properties;
            properties.push(format!(
                "Filter: {}",
                render::scalar(predicate, input.schema())
            ));
            walk(input, indent, properties, lines, analyze, actual_rows);
        }
        Plan::Project { input, .. } => {
            if properties.is_empty() {
                walk(input, indent, properties, lines, analyze, actual_rows);
            } else {
                emit(
                    plan,
                    "Result",
                    indent,
                    properties,
                    lines,
                    analyze,
                    actual_rows,
                );
                walk(input, indent + 1, Vec::new(), lines, analyze, None);
            }
        }
        Plan::CatalogScan(scan) => emit(
            plan,
            &format!("Seq Scan on {}", scan.source.name()),
            indent,
            properties,
            lines,
            analyze,
            actual_rows,
        ),
        Plan::TableFunction(function) => emit(
            plan,
            &format!("Function Scan on {}", function.function.name()),
            indent,
            properties,
            lines,
            analyze,
            actual_rows,
        ),
        Plan::Scan(scan) => {
            let mut properties = properties;
            properties.extend(index_properties(scan));
            emit(
                plan,
                &format!(
                    "{} Scan on {}",
                    if scan.index.is_some() { "Index" } else { "Seq" },
                    scan.table.def().name
                ),
                indent,
                properties,
                lines,
                analyze,
                actual_rows,
            );
        }
        Plan::Alias { input, schema } => {
            let name = schema
                .first()
                .and_then(|field| field.qualifier.clone())
                .unwrap_or_else(|| "subquery".to_string());
            emit(
                plan,
                &format!("Subquery Scan on {name}"),
                indent,
                properties,
                lines,
                analyze,
                actual_rows,
            );
            walk(input, indent + 1, Vec::new(), lines, analyze, None);
        }
        Plan::Join {
            left,
            right,
            join_type,
            on,
            ..
        } => {
            let mut properties = properties;
            if let Some(on) = on {
                let mut schema = left.schema().to_vec();
                schema.extend(right.schema().to_vec());
                properties.push(format!("Join Filter: {}", render::scalar(on, &schema)));
            }
            let label = match join_type {
                vsql_parser::JoinType::Inner => "Nested Loop",
                vsql_parser::JoinType::Left => "Nested Loop Left Join",
                vsql_parser::JoinType::Right => "Nested Loop Right Join",
                vsql_parser::JoinType::Full => "Nested Loop Full Join",
                vsql_parser::JoinType::Cross => "Nested Loop",
            };
            emit(plan, label, indent, properties, lines, analyze, actual_rows);
            walk(left, indent + 1, Vec::new(), lines, analyze, None);
            walk(right, indent + 1, Vec::new(), lines, analyze, None);
        }
        Plan::Sort { input, keys } => {
            let mut properties = properties;
            let schema = input.schema();
            let keys: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{} {} NULLS {}",
                        render::scalar(&key.expr, schema),
                        if key.asc { "ASC" } else { "DESC" },
                        if key.nulls_first { "FIRST" } else { "LAST" }
                    )
                })
                .collect();
            properties.push(format!("Sort Key: {}", keys.join(", ")));
            emit(
                plan,
                "Sort",
                indent,
                properties,
                lines,
                analyze,
                actual_rows,
            );
            walk(input, indent + 1, Vec::new(), lines, analyze, None);
        }
        Plan::Aggregate {
            input,
            groups,
            aggregates,
            ..
        } => {
            let mut properties = properties;
            let schema = input.schema();
            if !groups.is_empty() {
                properties.push(format!(
                    "Group Key: {}",
                    groups
                        .iter()
                        .map(|group| render::scalar(group, schema))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let label = if groups.is_empty() {
                "Aggregate"
            } else {
                "HashAggregate"
            };
            emit(plan, label, indent, properties, lines, analyze, actual_rows);
            let _ = aggregates;
            walk(input, indent + 1, Vec::new(), lines, analyze, None);
        }
        Plan::Distinct { input, on } => {
            let mut properties = properties;
            properties.push(format!(
                "Unique Key: {}",
                on.iter()
                    .map(|expr| render::scalar(expr, input.schema()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            emit(
                plan,
                "Unique",
                indent,
                properties,
                lines,
                analyze,
                actual_rows,
            );
            walk(input, indent + 1, Vec::new(), lines, analyze, None);
        }
        Plan::Limit {
            input,
            limit,
            offset,
        } => {
            let mut properties = properties;
            if let Some(limit) = limit {
                properties.push(format!("Limit: {limit}"));
            }
            if *offset > 0 {
                properties.push(format!("Offset: {offset}"));
            }
            emit(
                plan,
                "Limit",
                indent,
                properties,
                lines,
                analyze,
                actual_rows,
            );
            walk(input, indent + 1, Vec::new(), lines, analyze, None);
        }
        Plan::SetOp {
            left,
            right,
            op,
            all,
            ..
        } => {
            let label = match op {
                SetOperator::Union => "Union",
                SetOperator::Intersect => "Intersect",
                SetOperator::Except => "Except",
            };
            let label = if *all {
                label.to_string()
            } else {
                format!("{label} (unique)")
            };
            emit(
                plan,
                &label,
                indent,
                properties,
                lines,
                analyze,
                actual_rows,
            );
            walk(left, indent + 1, Vec::new(), lines, analyze, None);
            walk(right, indent + 1, Vec::new(), lines, analyze, None);
        }
        Plan::SingleRow { .. } => emit(
            plan,
            "Result",
            indent,
            properties,
            lines,
            analyze,
            actual_rows,
        ),
        Plan::Values { .. } => emit(
            plan,
            "Values",
            indent,
            properties,
            lines,
            analyze,
            actual_rows,
        ),
    }
}

fn index_properties(scan: &ScanPlan) -> Vec<String> {
    let Some(index) = &scan.index else {
        return Vec::new();
    };
    let mut properties = vec![format!("Index Name: {}", index.index.name)];
    if let Some(key) = index.keys.first() {
        // The probe key is a constant by construction (that is what makes it
        // usable as an index probe), so it is rendered against an empty schema.
        // The column name comes from the index definition, which is what turns
        // `Index Cond: 1` into the readable `Index Cond: (user_id = 1)`.
        let def = scan.table.def();
        let column = index
            .index
            .columns
            .first()
            .and_then(|ordinal| def.columns.get(*ordinal))
            .map(|column| column.name.clone())
            .unwrap_or_else(|| "?".to_string());
        properties.push(format!(
            "Index Cond: ({} = {})",
            column,
            render::scalar(key, &[])
        ));
    }
    properties
}

fn emit(
    plan: &Plan,
    label: &str,
    indent: usize,
    properties: Vec<String>,
    lines: &mut Vec<String>,
    analyze: bool,
    actual_rows: Option<u64>,
) {
    let rows = estimate_rows(plan);
    let width = schema_width(plan.schema());
    let mut line = format!(
        "{}{}{}  (cost=0.00..0.00 rows={rows} width={width}",
        "  ".repeat(indent),
        if indent == 0 { "" } else { "->  " },
        label
    );
    if analyze {
        let actual = actual_rows.unwrap_or(0);
        line.push_str(&format!(" actual time=0.000..0.000 rows={actual} loops=1"));
    }
    line.push(')');
    lines.push(line);
    for property in properties {
        lines.push(format!("{}{}", "  ".repeat(indent + 1), property));
    }
}
