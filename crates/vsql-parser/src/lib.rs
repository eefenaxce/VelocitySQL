//! VelocitySQL's SQL front-end.
//!
//! # Pipeline
//!
//! ```text
//! SQL text ──► sqlparser-rs (PostgreSQL dialect) ──► lowering ──► crate::ast
//! ```
//!
//! `sqlparser-rs` gives us PostgreSQL-compatible *syntax*; the lowering pass
//! turns its dialect-neutral tree into [`ast`], the closed AST the planner and
//! executor consume. Constructs we cannot execute yet are rejected during
//! lowering with [`ParserError::NotSupported`], never silently ignored.
//!
//! Errors are shaped like PostgreSQL's:
//!
//! ```
//! use vsql_parser::{parse, ParserError};
//! let err = parse("SELECT FROM").unwrap_err();
//! assert!(matches!(err, ParserError::Syntax { .. }));
//! assert!(err.to_string().starts_with("syntax error at or near"));
//! ```
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod ast;
pub mod error;
mod lower;

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Location, Token, Tokenizer};

pub use ast::*;
pub use error::ParserError;

/// Every word the parser recognises, paired with `true` when PostgreSQL
/// reserves it for a table alias.
///
/// Reserving a word for a table alias is the strictest reservation PostgreSQL
/// applies, so it is what `pg_get_keywords()` reports as `catcode = 'R'` and
/// what decides whether a bare alias needs `AS`.
pub fn keywords() -> impl Iterator<Item = (&'static str, bool)> {
    sqlparser::keywords::ALL_KEYWORDS.iter().map(|word| {
        let reserved = sqlparser::keywords::RESERVED_FOR_TABLE_ALIAS
            .iter()
            .any(|keyword| format!("{keyword:?}").eq_ignore_ascii_case(word));
        (*word, reserved)
    })
}

/// Parses one or more SQL statements.
///
/// The returned statements are already lowered into [`ast`], with identifiers
/// normalised the way PostgreSQL normalises them.
pub fn parse(sql: &str) -> Result<Vec<Statement>, ParserError> {
    // A UTF-8 byte order mark is common in files written on Windows, and a piped
    // script can even arrive with more than one. It is invisible to the reader but
    // would be reported as the offending token of a `syntax error at or near ""`,
    // so every leading mark is stripped before tokenising.
    let sql = sql.trim_start_matches('\u{feff}');
    let (sql, create_user) = rewrite_create_user(sql);
    let dialect = PostgreSqlDialect {};
    let parsed = Parser::parse_sql(&dialect, &sql).map_err(|e| ParserError::from_sqlparser(&e))?;
    lower::lower_statements_with(&parsed, &create_user)
}

/// Rewrites `CREATE USER` into `CREATE ROLE` before parsing.
///
/// `sqlparser` implements `CREATE USER` as the Snowflake key/value form
/// (`CREATE USER x PASSWORD = 'y'`) and its PostgreSQL dialect rejects
/// `CREATE USER x PASSWORD 'y'` outright, while PostgreSQL itself treats
/// `CREATE USER` as `CREATE ROLE` whose `LOGIN` attribute defaults to true.
/// The keyword is therefore swapped here 鈥?at the *token* level, so a `USER`
/// inside a string literal or an identifier is never touched 鈥?and the fact that
/// it was a `CREATE USER` is reported back per statement.
///
/// `USER` and `ROLE` are both four bytes, so the splice cannot move any other
/// offset, and the rewritten text keeps the line and column numbering of the
/// original.
fn rewrite_create_user(sql: &str) -> (String, Vec<bool>) {
    let dialect = PostgreSqlDialect {};
    let Ok(tokens) = Tokenizer::new(&dialect, sql).tokenize_with_location() else {
        return (sql.to_string(), Vec::new());
    };

    let mut flags: Vec<bool> = Vec::new();
    let mut edits: Vec<usize> = Vec::new();
    let mut statement = 0usize;
    let mut at_statement_start = true;
    let mut after_create = false;

    for token in &tokens {
        // The located tokenizer keeps whitespace, which must not be mistaken for
        // the end of a statement nor for a step between `CREATE` and `USER`.
        if matches!(token.token, Token::Whitespace(_)) {
            continue;
        }
        if matches!(token.token, Token::SemiColon) {
            statement += 1;
            at_statement_start = true;
            after_create = false;
            continue;
        }
        let keyword = match &token.token {
            Token::Word(word) => Some(word.keyword),
            _ => None,
        };
        if at_statement_start {
            at_statement_start = false;
            while flags.len() <= statement {
                flags.push(false);
            }
            after_create = keyword == Some(Keyword::CREATE);
            continue;
        }
        if after_create && keyword == Some(Keyword::USER) {
            if let Some(offset) = byte_offset(sql, &token.span.start) {
                if sql[offset..].len() >= 4 {
                    flags[statement] = true;
                    edits.push(offset);
                }
            }
            after_create = false;
            continue;
        }
        after_create = false;
    }

    if edits.is_empty() {
        return (sql.to_string(), flags);
    }
    let mut rewritten = sql.to_string();
    for offset in edits {
        // Guard against a token whose text is not the keyword we matched.
        if rewritten
            .get(offset..offset + 4)
            .map(|text| text.eq_ignore_ascii_case("user"))
            .unwrap_or(false)
        {
            rewritten.replace_range(offset..offset + 4, "ROLE");
        }
    }
    (rewritten, flags)
}

/// Converts a one-based `(line, column)` pair into a byte offset.
///
/// Columns count *characters*, so a naive `line * column` arithmetic would be
/// wrong the moment the query contains a non-ASCII character.
fn byte_offset(sql: &str, location: &Location) -> Option<usize> {
    if location.line == 0 {
        return None;
    }
    let mut offset = 0usize;
    for (index, line) in sql.split('\n').enumerate() {
        if index as u64 + 1 == location.line {
            let column = location.column.saturating_sub(1) as usize;
            return Some(match line.char_indices().nth(column) {
                Some((byte, _)) => offset + byte,
                None => offset + line.len(),
            });
        }
        offset += line.len() + 1;
    }
    None
}

/// Parses exactly one statement, rejecting empty and multi-statement input.
pub fn parse_statement(sql: &str) -> Result<Statement, ParserError> {
    let mut statements = parse(sql)?;
    match statements.len() {
        1 => Ok(statements.remove(0)),
        0 => Err(ParserError::syntax_at("end of input", None)),
        n => Err(ParserError::Invalid(format!(
            "expected a single statement, found {n}"
        ))),
    }
}

/// Parses a bare expression, e.g. `1 + 2` or `col::text`.
///
/// Used for `DEFAULT` values and for `SET` assignments.
pub fn parse_expression(sql: &str) -> Result<Expr, ParserError> {
    let sql = sql.trim_start_matches('\u{feff}');
    let dialect = PostgreSqlDialect {};
    let parser = Parser::new(&dialect);
    let mut parser = parser
        .try_with_sql(sql)
        .map_err(|e| ParserError::from_sqlparser(&e))?;
    let expr = parser
        .parse_expr()
        .map_err(|e| ParserError::from_sqlparser(&e))?;
    lower::lower_expr_public(&expr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vsql_types::{DataType, Value};

    fn one(sql: &str) -> Statement {
        parse_statement(sql).unwrap_or_else(|e| panic!("failed to parse `{sql}`: {e}"))
    }

    #[test]
    fn folds_unquoted_identifiers_but_keeps_quoted_ones() {
        let Statement::Query(q) = one("SELECT Foo, \"Bar\" FROM Tbl") else {
            panic!("expected a query");
        };
        let SetExpr::Select(select) = q.body else {
            panic!("expected a select");
        };
        let SelectItem::Expr { expr, .. } = &select.projection[0] else {
            panic!("expected an expression");
        };
        assert_eq!(
            *expr,
            Expr::Column(ColumnRef {
                qualifier: None,
                name: "foo".into(),
                quoted: false,
            })
        );
        let SelectItem::Expr { expr, .. } = &select.projection[1] else {
            panic!("expected an expression");
        };
        assert_eq!(
            *expr,
            Expr::Column(ColumnRef {
                qualifier: None,
                name: "Bar".into(),
                quoted: true,
            })
        );
        let TableFactor::Table { name, .. } = &select.from[0].relation else {
            panic!("expected a table");
        };
        assert_eq!(name, &vec!["tbl".to_string()]);
    }

    #[test]
    fn numeric_literals_get_their_postgres_type() {
        let Statement::Query(q) = one("SELECT 1, 3000000000, 1.5") else {
            panic!("expected a query");
        };
        let SetExpr::Select(select) = q.body else {
            panic!("expected a select");
        };
        let exprs: Vec<Value> = select
            .projection
            .iter()
            .map(|item| match item {
                SelectItem::Expr {
                    expr: Expr::Literal(v),
                    ..
                } => v.clone(),
                other => panic!("unexpected item {other:?}"),
            })
            .collect();
        assert_eq!(exprs[0], Value::Int4(1));
        assert_eq!(exprs[1], Value::Int8(3_000_000_000));
        assert_eq!(exprs[2], Value::Numeric("1.5".parse().unwrap()));
    }

    #[test]
    fn select_with_join_group_and_order() {
        let Statement::Query(q) = one("SELECT u.id, count(*) AS n \
             FROM users u LEFT JOIN orders o ON o.user_id = u.id \
             WHERE u.active AND o.total > 10 \
             GROUP BY u.id HAVING count(*) > 1 \
             ORDER BY n DESC NULLS LAST LIMIT 10 OFFSET 5")
        else {
            panic!("expected a query");
        };
        assert_eq!(q.limit, Some(Expr::Literal(Value::Int4(10))));
        assert_eq!(q.offset, Some(Expr::Literal(Value::Int4(5))));
        let SetExpr::Select(select) = &q.body else {
            panic!("expected a select");
        };
        assert_eq!(select.from.len(), 1);
        assert_eq!(select.from[0].joins.len(), 1);
        assert_eq!(select.from[0].joins[0].op, JoinType::Left);
        assert_eq!(select.group_by.len(), 1);
        assert!(select.having.is_some());
        assert!(!q.order_by[0].asc);
        assert_eq!(q.order_by[0].nulls_first, Some(false));
    }

    #[test]
    fn create_table_with_constraints() {
        let Statement::CreateTable(table) = one("CREATE TABLE IF NOT EXISTS app.users ( \
                 id INT PRIMARY KEY, \
                 email VARCHAR(255) NOT NULL UNIQUE, \
                 created_at TIMESTAMPTZ DEFAULT now(), \
                 balance NUMERIC(12,2) DEFAULT 0 \
             )")
        else {
            panic!("expected a create table");
        };
        assert_eq!(table.name, vec!["app".to_string(), "users".to_string()]);
        assert!(table.if_not_exists);
        assert_eq!(table.columns.len(), 4);
        assert_eq!(table.columns[0].data_type, DataType::Int4);
        assert!(table.columns[0].primary_key);
        assert!(!table.columns[0].nullable);
        assert_eq!(table.columns[1].data_type, DataType::Varchar(Some(255)));
        assert!(table.columns[1].unique);
        assert_eq!(
            table.columns[3].data_type,
            DataType::Numeric {
                precision: Some(12),
                scale: Some(2)
            }
        );
    }

    #[test]
    fn insert_values_and_returning() {
        let Statement::Insert(insert) =
            one("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y') RETURNING a")
        else {
            panic!("expected an insert");
        };
        assert_eq!(insert.columns, vec!["a".to_string(), "b".to_string()]);
        let InsertSource::Values(rows) = &insert.source else {
            panic!("expected VALUES");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], Expr::Literal(Value::Int4(2)));
        assert_eq!(insert.returning.len(), 1);
    }

    #[test]
    fn typed_literals_and_casts() {
        let Statement::Query(q) = one("SELECT DATE '2024-01-01', '5'::int, 1 + INTERVAL '1 day'")
        else {
            panic!("expected a query");
        };
        let SetExpr::Select(select) = q.body else {
            panic!("expected a select");
        };
        let SelectItem::Expr { expr, .. } = &select.projection[0] else {
            panic!("expected an expression");
        };
        assert!(matches!(expr, Expr::Cast { .. }));
        let SelectItem::Expr { expr, .. } = &select.projection[2] else {
            panic!("expected an expression");
        };
        let Expr::Binary { right, .. } = expr else {
            panic!("expected a binary op, got {expr:?}");
        };
        assert!(matches!(**right, Expr::Literal(Value::Interval(_))));
    }

    #[test]
    fn unsupported_constructs_are_rejected_loudly() {
        let err = parse_statement("SELECT row_number() OVER () FROM t").unwrap_err();
        assert!(matches!(err, ParserError::NotSupported(_)), "{err}");
        let err = parse_statement("CREATE TABLE t (a INT) INHERITS (parent)").unwrap_err();
        assert!(matches!(err, ParserError::NotSupported(_)), "{err}");
    }

    #[test]
    fn error_messages_look_like_postgres() {
        let err = parse("SELECT 1 +").unwrap_err();
        assert!(
            err.to_string().starts_with("syntax error at or near"),
            "{err}"
        );
        // The token extraction and the `POSITION` field are what the wire
        // protocol forwards to clients.
        let synthetic = ParserError::syntax_at("FROM", Some(7));
        assert_eq!(synthetic.to_string(), "syntax error at or near \"FROM\"");
        assert_eq!(synthetic.position(), Some(7));
    }

    #[test]
    fn parameter_count_counts_placeholders() {
        let count = |sql: &str| one(sql).parameter_count();
        assert_eq!(count("SELECT a FROM t"), 0);
        assert_eq!(count("SELECT 1 + $1"), 1);
        // The count is one past the *highest* index, however they are ordered.
        assert_eq!(count("SELECT $3, $1"), 3);
        // A `$` inside a literal is text, not a placeholder.
        assert_eq!(count("SELECT 'no $1 here'"), 0);
        // Every clause is walked, including subqueries.
        assert_eq!(count("SELECT (SELECT $3) FROM t WHERE a IN ($1, $2)"), 3);
        assert_eq!(count("INSERT INTO t VALUES ($1, $2)"), 2);
        assert_eq!(count("UPDATE t SET a = $1 WHERE b = $2 RETURNING $3"), 3);
        assert_eq!(count("DELETE FROM t WHERE a = $1"), 1);
        assert_eq!(count("EXPLAIN SELECT $1"), 1);
    }

    #[test]
    fn multiple_statements() {
        let statements = parse("CREATE TABLE t (a INT); INSERT INTO t VALUES (1);").unwrap();
        assert_eq!(statements.len(), 2);
        assert!(matches!(statements[0], Statement::CreateTable(_)));
        assert!(matches!(statements[1], Statement::Insert(_)));
    }

    #[test]
    fn expression_entry_point() {
        assert_eq!(
            parse_expression("42").unwrap(),
            Expr::Literal(Value::Int4(42))
        );
        assert!(parse_expression("a +").is_err());
    }
    #[test]
    fn create_user_is_create_role_with_login() {
        let Statement::CreateRole {
            name,
            if_not_exists,
            options,
        } = one("CREATE USER app PASSWORD 'secret'")
        else {
            panic!("expected a role creation");
        };
        assert_eq!(name, "app");
        assert!(!if_not_exists);
        assert!(options.contains(&RoleOption::Login(true)), "{options:?}");
        assert!(options.contains(&RoleOption::Password(Some("secret".into()))));

        // `CREATE ROLE` keeps PostgreSQL's NOLOGIN default.
        let Statement::CreateRole { options, .. } = one("CREATE ROLE app") else {
            panic!("expected a role creation");
        };
        assert!(options.is_empty(), "{options:?}");

        // An explicit NOLOGIN must survive the rewrite.
        let Statement::CreateRole { options, .. } = one("CREATE USER app NOLOGIN") else {
            panic!("expected a role creation");
        };
        assert_eq!(options, vec![RoleOption::Login(false)]);

        // A `USER` keyword inside a literal is text, not syntax.
        let Statement::Query(query) = one("SELECT 'CREATE USER x'") else {
            panic!("expected a query");
        };
        let SetExpr::Select(select) = query.body else {
            panic!("expected a select");
        };
        let SelectItem::Expr { expr, .. } = &select.projection[0] else {
            panic!("expected an expression");
        };
        assert!(
            matches!(expr, Expr::Literal(Value::Text(text)) if text == "CREATE USER x"),
            "{expr:?}"
        );
    }

    #[test]
    fn role_ddl_carries_its_attributes() {
        let Statement::CreateRole {
            name,
            if_not_exists,
            options,
        } = one("CREATE ROLE IF NOT EXISTS admin SUPERUSER CREATEDB LOGIN PASSWORD 's3cret'")
        else {
            panic!("expected a role creation");
        };
        assert_eq!(name, "admin");
        assert!(if_not_exists);
        assert_eq!(
            options,
            vec![
                RoleOption::Superuser(true),
                RoleOption::Login(true),
                RoleOption::CreateDb(true),
                RoleOption::Password(Some("s3cret".into())),
            ]
        );

        let Statement::AlterRole { name, options } = one("ALTER USER app PASSWORD NULL") else {
            panic!("expected a role change");
        };
        assert_eq!(name, "app");
        assert_eq!(options, vec![RoleOption::Password(None)]);

        let Statement::DropRole { if_exists, names } = one("DROP USER IF EXISTS app, bob") else {
            panic!("expected a role drop");
        };
        assert!(if_exists);
        assert_eq!(names, vec!["app", "bob"]);
    }

    #[test]
    fn unsupported_role_options_are_refused() {
        for sql in [
            "CREATE ROLE app INHERIT",
            "CREATE ROLE app CONNECTION LIMIT 5",
            "CREATE ROLE app VALID UNTIL '2030-01-01'",
            "CREATE ROLE app IN ROLE other",
            "ALTER ROLE app RENAME TO other",
        ] {
            let error = parse_statement(sql).unwrap_err();
            assert!(
                error.to_string().contains("feature not supported"),
                "`{sql}` should be refused explicitly, got: {error}"
            );
        }
    }
}
