//! SQL text rendering.
//!
//! Two things need to turn a tree back into SQL:
//!
//! * **Stored `DEFAULT` expressions.** The catalog keeps them as text so that
//!   they survive a snapshot and can be re-evaluated per inserted row.
//! * **`EXPLAIN`.** Printing the bound expression (with resolved column names)
//!   is what makes a plan readable, and it doubles as a debugging aid.
//!
//! Rendering is deliberately simple: parenthesise aggressively rather than
//! trying to minimise parentheses, because the output is either re-parsed or
//! read by a human, and both prefer unambiguity.

use vsql_parser::ast::{self, BinaryOp, Expr, UnaryOp};
use vsql_types::Value;

use crate::expr::{AnyAllSource, ScalarExpr};
use crate::field::Field;

/// Renders a parsed expression as SQL.
pub fn ast_expr(expr: &Expr) -> String {
    match expr {
        Expr::Literal(value) => literal(value),
        Expr::Column(reference) => match &reference.qualifier {
            Some(qualifier) => format!("{qualifier}.{}", reference.name),
            None => reference.name.clone(),
        },
        Expr::Nested(inner) => format!("({})", ast_expr(inner)),
        Expr::Subscript { expr, index } => {
            format!("{}[{}]", ast_expr(expr), ast_expr(index))
        }
        Expr::Unary { op, expr } => match op {
            UnaryOp::Not => format!("NOT {}", ast_expr(expr)),
            UnaryOp::Negate => format!("-{}", ast_expr(expr)),
            UnaryOp::Plus => format!("+{}", ast_expr(expr)),
            UnaryOp::BitNot => format!("~{}", ast_expr(expr)),
        },
        Expr::Binary { left, op, right } => format!(
            "({} {} {})",
            ast_expr(left),
            op_symbol(*op),
            ast_expr(right)
        ),
        Expr::IsNull { expr, negated } => format!(
            "{} IS {}NULL",
            ast_expr(expr),
            if *negated { "NOT " } else { "" }
        ),
        Expr::IsBool {
            expr,
            value,
            negated,
        } => {
            let keyword = match value {
                Some(true) => "TRUE",
                Some(false) => "FALSE",
                None => "UNKNOWN",
            };
            format!(
                "{} IS {}{}",
                ast_expr(expr),
                if *negated { "NOT " } else { "" },
                keyword
            )
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => format!(
            "{} {}IN ({})",
            ast_expr(expr),
            if *negated { "NOT " } else { "" },
            list.iter().map(ast_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::InSubquery {
            expr,
            negated,
            subquery: _,
        } => format!(
            "{} {}IN (<subquery>)",
            ast_expr(expr),
            if *negated { "NOT " } else { "" }
        ),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => format!(
            "{} {}BETWEEN {} AND {}",
            ast_expr(expr),
            if *negated { "NOT " } else { "" },
            ast_expr(low),
            ast_expr(high)
        ),
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
            escape: _,
        } => format!(
            "{} {}{} {}",
            ast_expr(expr),
            if *negated { "NOT " } else { "" },
            if *case_insensitive { "ILIKE" } else { "LIKE" },
            ast_expr(pattern)
        ),
        Expr::Cast {
            expr, data_type, ..
        } => format!("CAST({} AS {})", ast_expr(expr), data_type.format_type()),
        Expr::Function(call) => {
            let args: Vec<String> = call.args.iter().map(ast_expr).collect();
            let mut inner = args.join(", ");
            if call.star {
                inner = "*".to_string();
            } else if call.distinct {
                inner = format!("DISTINCT {inner}");
            }
            format!("{}({inner})", call.name)
        }
        Expr::Case {
            operand,
            whens,
            else_result,
        } => {
            let mut text = String::from("CASE");
            if let Some(operand) = operand {
                text.push(' ');
                text.push_str(&ast_expr(operand));
            }
            for (condition, result) in whens {
                text.push_str(&format!(
                    " WHEN {} THEN {}",
                    ast_expr(condition),
                    ast_expr(result)
                ));
            }
            if let Some(else_result) = else_result {
                text.push_str(&format!(" ELSE {}", ast_expr(else_result)));
            }
            text.push_str(" END");
            text
        }
        Expr::Subquery(_) => "(<subquery>)".to_string(),
        Expr::Exists { negated, .. } => {
            format!("{}EXISTS (<subquery>)", if *negated { "NOT " } else { "" })
        }
        Expr::AnyAll {
            left,
            op,
            is_all,
            right: _,
        } => format!(
            "{} {} {} (<subquery>)",
            ast_expr(left),
            op_symbol(*op),
            if *is_all { "ALL" } else { "ANY" }
        ),
        Expr::Array(items) => format!(
            "ARRAY[{}]",
            items.iter().map(ast_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::Row(items) => format!(
            "({})",
            items.iter().map(ast_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::Wildcard => "*".to_string(),
        Expr::QualifiedWildcard(qualifier) => format!("{qualifier}.*"),
        Expr::Parameter(index) => format!("${}", index + 1),
    }
}

/// Renders a *bound* expression, resolving slot indices to column names.
pub fn scalar(expr: &ScalarExpr, schema: &[Field]) -> String {
    let column = |index: &usize| {
        schema
            .get(*index)
            .map(|field| match &field.qualifier {
                Some(qualifier) => format!("{qualifier}.{}", field.name),
                None => field.name.clone(),
            })
            .unwrap_or_else(|| format!("?column{index}?"))
    };
    match expr {
        ScalarExpr::Const(value) => literal(value),
        ScalarExpr::Column(index) => column(index),
        ScalarExpr::Aggregate(index) => format!("<agg{}>", column(index)),
        ScalarExpr::Outer { levels_up, index } => format!("<outer {levels_up}:{index}>"),
        ScalarExpr::Unary { op, expr } => match op {
            UnaryOp::Not => format!("NOT {}", scalar(expr, schema)),
            UnaryOp::Negate => format!("-{}", scalar(expr, schema)),
            _ => scalar(expr, schema),
        },
        ScalarExpr::Binary { op, left, right } => format!(
            "({} {} {})",
            scalar(left, schema),
            op_symbol(*op),
            scalar(right, schema)
        ),
        ScalarExpr::Cast {
            expr, data_type, ..
        } => format!("({})::{}", scalar(expr, schema), data_type.format_type()),
        ScalarExpr::IsNull { expr, negated } => format!(
            "{} IS {}NULL",
            scalar(expr, schema),
            if *negated { "NOT " } else { "" }
        ),
        ScalarExpr::IsBool {
            expr,
            value,
            negated,
        } => format!(
            "{} IS {}{}",
            scalar(expr, schema),
            if *negated { "NOT " } else { "" },
            match value {
                Some(true) => "TRUE",
                Some(false) => "FALSE",
                None => "UNKNOWN",
            }
        ),
        ScalarExpr::IsDistinctFrom {
            left,
            right,
            negated,
        } => format!(
            "{} IS {}DISTINCT FROM {}",
            scalar(left, schema),
            if *negated { "NOT " } else { "" },
            scalar(right, schema)
        ),
        ScalarExpr::InList {
            expr,
            list,
            negated,
        } => format!(
            "{} {}IN ({})",
            scalar(expr, schema),
            if *negated { "NOT " } else { "" },
            list.iter()
                .map(|item| scalar(item, schema))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ScalarExpr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => format!(
            "{} {}{} {}",
            scalar(expr, schema),
            if *negated { "NOT " } else { "" },
            if *case_insensitive { "ILIKE" } else { "LIKE" },
            scalar(pattern, schema)
        ),
        ScalarExpr::Between {
            expr,
            low,
            high,
            negated,
        } => format!(
            "{} {}BETWEEN {} AND {}",
            scalar(expr, schema),
            if *negated { "NOT " } else { "" },
            scalar(low, schema),
            scalar(high, schema)
        ),
        ScalarExpr::Case {
            operand,
            whens,
            else_result,
        } => {
            let mut text = String::from("CASE");
            if let Some(operand) = operand {
                text.push_str(&format!(" {}", scalar(operand, schema)));
            }
            for (condition, result) in whens {
                text.push_str(&format!(
                    " WHEN {} THEN {}",
                    scalar(condition, schema),
                    scalar(result, schema)
                ));
            }
            if let Some(else_result) = else_result {
                text.push_str(&format!(" ELSE {}", scalar(else_result, schema)));
            }
            text.push_str(" END");
            text
        }
        ScalarExpr::Function { name, args } => format!(
            "{}({})",
            name,
            args.iter()
                .map(|arg| scalar(arg, schema))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ScalarExpr::Array(items) => format!(
            "ARRAY[{}]",
            items
                .iter()
                .map(|item| scalar(item, schema))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ScalarExpr::Row(items) => format!(
            "({})",
            items
                .iter()
                .map(|item| scalar(item, schema))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ScalarExpr::Subscript { expr, index } => {
            format!("{}[{}]", scalar(expr, schema), scalar(index, schema))
        }
        ScalarExpr::ScalarSubquery(_) => "(<subquery>)".to_string(),
        ScalarExpr::Exists { negated, .. } => {
            format!("{}EXISTS (<subquery>)", if *negated { "NOT " } else { "" })
        }
        ScalarExpr::InSubquery { expr, negated, .. } => format!(
            "{} {}IN (<subquery>)",
            scalar(expr, schema),
            if *negated { "NOT " } else { "" }
        ),
        ScalarExpr::Parameter(index) => format!("${}", index + 1),
        ScalarExpr::AnyAll {
            left,
            op,
            is_all,
            source,
        } => format!(
            "{} {} {} ({})",
            scalar(left, schema),
            op_symbol(*op),
            if *is_all { "ALL" } else { "ANY" },
            match source {
                AnyAllSource::Array(array) => scalar(array, schema),
                AnyAllSource::Subquery(_) => "(<subquery>)".to_string(),
            }
        ),
    }
}

/// Renders a value the way it would be written as a literal.
pub fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Text(text) => format!("'{}'", text.replace('\'', "''")),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Date(_) | Value::Time(_) | Value::Timestamp(_) | Value::Timestamptz(_) => {
            format!("'{}'", value)
        }
        Value::Interval(interval) => format!("INTERVAL '{}'", interval),
        Value::Bytes(bytes) => format!("'\\x{}'", hex(bytes)),
        Value::Array(items) => format!(
            "ARRAY[{}]",
            items.iter().map(literal).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The SQL spelling of a binary operator.
pub fn op_symbol(op: BinaryOp) -> &'static str {
    match op {
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
        BinaryOp::IsDistinctFrom => "IS DISTINCT FROM",
        BinaryOp::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
        BinaryOp::RegexMatch => "~",
        BinaryOp::RegexIMatch => "~*",
        BinaryOp::RegexNotMatch => "!~",
        BinaryOp::RegexNotIMatch => "!~*",
        BinaryOp::Pow => "^",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "#",
        BinaryOp::ShiftLeft => "<<",
        BinaryOp::ShiftRight => ">>",
    }
}

/// Renders a whole `SELECT` for diagnostics.
pub fn query(query: &ast::Query) -> String {
    match &query.body {
        ast::SetExpr::Select(select) => {
            let projection = select
                .projection
                .iter()
                .map(|item| match item {
                    ast::SelectItem::Expr { expr, alias } => match alias {
                        Some(alias) => format!("{} AS {alias}", ast_expr(expr)),
                        None => ast_expr(expr),
                    },
                    ast::SelectItem::Wildcard => "*".to_string(),
                    ast::SelectItem::QualifiedWildcard(name) => format!("{name}.*"),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let mut text = format!("SELECT {projection}");
            if !select.from.is_empty() {
                text.push_str(&format!(
                    " FROM {}",
                    select
                        .from
                        .iter()
                        .map(|item| match &item.relation {
                            ast::TableFactor::Table { name, .. } => name.join("."),
                            _ => "(...)".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some(selection) = &select.selection {
                text.push_str(&format!(" WHERE {}", ast_expr(selection)));
            }
            text
        }
        _ => "(query)".to_string(),
    }
}
