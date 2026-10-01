//! End-to-end tests for milestone M1.
//!
//! These exercise the whole stack through the public API — SQL text in, rows out
//! — which is the only level at which "PostgreSQL compatible" is a meaningful
//! claim. Each test states the PostgreSQL behaviour it pins down.

use std::sync::Arc;

use vsql_executor::{Engine, ExecError, QueryResult, Session};
use vsql_types::{DataType, Value};

fn session() -> Session {
    Session::new(Arc::new(Engine::new()))
}

fn run(session: &mut Session, sql: &str) -> QueryResult {
    let mut results = session
        .execute(sql)
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
    assert_eq!(results.len(), 1, "expected a single result for `{sql}`");
    results.remove(0)
}

fn rows(session: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    run(session, sql).data().to_vec()
}

fn text_rows(session: &mut Session, sql: &str) -> Vec<String> {
    rows(session, sql)
        .into_iter()
        .map(|row| {
            row.iter()
                .map(|value| {
                    if value.is_null() {
                        "NULL".to_string()
                    } else {
                        value.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Runs `sql` and returns the error it must produce.
///
/// Named `expect_*` so it can never collide with the idiomatic local binding
/// `error`: `let error = error(...)` would shadow the function for the whole
/// block, including the initialiser itself, and fail to compile.
fn expect_error(session: &mut Session, sql: &str) -> ExecError {
    match session.execute(sql) {
        Ok(_) => panic!("`{sql}` unexpectedly succeeded"),
        Err(error) => error,
    }
}

fn seeded() -> Session {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE users (
             id INT PRIMARY KEY,
             name TEXT NOT NULL,
             email TEXT UNIQUE,
             active BOOL DEFAULT true,
             created_at TIMESTAMPTZ DEFAULT now()
         )",
    );
    run(
        &mut session,
        "INSERT INTO users (id, name, email) VALUES
             (1, 'ada', 'ada@example.com'),
             (2, 'grace', 'grace@example.com'),
             (3, 'alan', NULL)",
    );
    run(
        &mut session,
        "CREATE TABLE orders (
             id INT PRIMARY KEY,
             user_id INT NOT NULL,
             total NUMERIC(10,2) NOT NULL
         )",
    );
    run(
        &mut session,
        "INSERT INTO orders VALUES (10, 1, 100.50), (11, 1, 20.00), (12, 2, 5.25)",
    );
    session
}

#[test]
fn create_insert_select_round_trip() {
    let mut session = seeded();
    assert_eq!(
        text_rows(&mut session, "SELECT id, name FROM users ORDER BY id"),
        vec!["1|ada", "2|grace", "3|alan"]
    );
    // `SELECT *` expands to the declaration order.
    let result = run(&mut session, "SELECT * FROM users WHERE id = 1");
    let names: Vec<&str> = result
        .schema()
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    assert_eq!(names, vec!["id", "name", "email", "active", "created_at"]);
}

#[test]
fn defaults_are_evaluated_per_row() {
    let mut session = seeded();
    let values = rows(&mut session, "SELECT active FROM users WHERE id = 3");
    assert_eq!(values, vec![vec![Value::Bool(true)]]);
    let created = rows(&mut session, "SELECT created_at FROM users WHERE id = 1");
    assert!(matches!(created[0][0], Value::Timestamptz(_)));
}

#[test]
fn where_order_limit_offset() {
    let mut session = seeded();
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT name FROM users WHERE id > 1 ORDER BY name DESC LIMIT 1 OFFSET 1"
        ),
        // `id > 1` keeps grace and alan; DESC orders them grace, alan; skipping
        // one row leaves alan.
        vec!["alan"]
    );
}

#[test]
fn null_ordering_matches_postgres() {
    let mut session = seeded();
    // ASC puts NULL last, DESC puts it first.
    assert_eq!(
        text_rows(&mut session, "SELECT email FROM users ORDER BY email ASC"),
        vec!["ada@example.com", "grace@example.com", "NULL"]
    );
    assert_eq!(
        text_rows(&mut session, "SELECT email FROM users ORDER BY email DESC"),
        vec!["NULL", "grace@example.com", "ada@example.com"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT email FROM users ORDER BY email ASC NULLS FIRST"
        ),
        vec!["NULL", "ada@example.com", "grace@example.com"]
    );
}

#[test]
fn aggregates_group_by_and_having() {
    let mut session = seeded();
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT count(*), count(email), sum(id) FROM users"
        ),
        vec!["3|2|6"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT user_id, sum(total) AS spent FROM orders
             GROUP BY user_id HAVING count(*) > 1 ORDER BY user_id"
        ),
        vec!["1|120.50"]
    );
    // An aggregate over an empty input still produces one row.
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM users WHERE id > 100"),
        vec!["0"]
    );
    assert_eq!(
        // `avg` returns `numeric`; the scale follows the division, so an exact
        // average prints without a fractional part.
        text_rows(&mut session, "SELECT min(id), max(id), avg(id) FROM users"),
        vec!["1|3|2"]
    );
}

#[test]
fn string_agg_joins_values_with_a_delimiter() {
    let mut session = seeded();
    assert_eq!(
        text_rows(&mut session, "SELECT string_agg(name, ', ') FROM users"),
        vec!["ada, grace, alan"]
    );
    // The delimiter is an expression evaluated per row and placed before that
    // row's value, as in PostgreSQL's transition function - so the first
    // value never carries one.
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT string_agg(name, ' | ' || id) FROM users WHERE id <= 2"
        ),
        vec!["ada | 2grace"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT user_id, string_agg(total::text, '+') FROM orders \
             GROUP BY user_id ORDER BY user_id"
        ),
        vec!["1|100.50+20.00", "2|5.25"]
    );
}

#[test]
fn grouping_by_an_expression_is_matched_structurally() {
    let mut session = seeded();
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT id + 1, count(*) FROM users GROUP BY id + 1 ORDER BY 1 LIMIT 2"
        ),
        vec!["2|1", "3|1"]
    );
}

#[test]
fn ungrouped_column_is_rejected_like_postgres() {
    let mut session = seeded();
    let error = expect_error(&mut session, "SELECT name FROM users GROUP BY id");
    assert_eq!(
        error.to_string(),
        "column \"name\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    assert_eq!(error.sqlstate(), "42803");
}

#[test]
fn joins() {
    let mut session = seeded();
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT u.name, o.total FROM users u JOIN orders o ON o.user_id = u.id
             ORDER BY o.id"
        ),
        vec!["ada|100.50", "ada|20.00", "grace|5.25"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT u.name, o.id FROM users u LEFT JOIN orders o ON o.user_id = u.id
             WHERE u.id = 3"
        ),
        vec!["alan|NULL"]
    );
    // RIGHT JOIN keeps the unmatched right rows.
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT o.id, u.name FROM orders o RIGHT JOIN users u ON o.user_id = u.id
             WHERE o.id IS NULL"
        ),
        vec!["NULL|alan"]
    );
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM users CROSS JOIN orders"),
        vec!["9"]
    );
}

#[test]
fn using_join_lists_the_key_once() {
    let mut session = seeded();
    let result = run(
        &mut session,
        "SELECT * FROM orders o JOIN users u USING (id) ORDER BY id",
    );
    let names: Vec<&str> = result
        .schema()
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    // `id` appears once, the remaining columns follow.
    assert_eq!(names[0], "id");
    assert_eq!(names.iter().filter(|name| **name == "id").count(), 1);
}

#[test]
fn distinct_and_set_operations() {
    let mut session = seeded();
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT DISTINCT user_id FROM orders ORDER BY user_id"
        ),
        vec!["1", "2"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT id FROM users WHERE id < 3
             UNION
             SELECT id FROM orders WHERE id > 11
             ORDER BY 1"
        ),
        vec!["1", "2", "12"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT id FROM users UNION ALL SELECT id FROM users WHERE id = 1"
        )
        .len(),
        4
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT id FROM users EXCEPT SELECT id FROM users WHERE id = 1"
        )
        .len(),
        2
    );
}

#[test]
fn subqueries_work_including_correlation() {
    let mut session = seeded();
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders) ORDER BY name"
        ),
        vec!["ada", "grace"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT name FROM users u WHERE EXISTS (
                 SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 50
             )"
        ),
        vec!["ada"]
    );
    // A scalar subquery in the projection.
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT (SELECT count(*) FROM orders o WHERE o.user_id = u.id) FROM users u ORDER BY u.id"
        ),
        vec!["2", "1", "0"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT name FROM (SELECT name FROM users WHERE id < 3) AS young ORDER BY name"
        ),
        vec!["ada", "grace"]
    );
}

#[test]
fn common_table_expressions() {
    let mut session = seeded();
    assert_eq!(
        text_rows(
            &mut session,
            "WITH big AS (SELECT * FROM orders WHERE total > 50)
             SELECT count(*) FROM big"
        ),
        vec!["1"]
    );
}

#[test]
fn update_delete_and_returning() {
    let mut session = seeded();
    let updated = run(
        &mut session,
        "UPDATE users SET name = 'ada lovelace', active = false WHERE id = 1 RETURNING name, active",
    );
    assert_eq!(updated.data().len(), 1);
    assert_eq!(updated.data()[0][0], Value::Text("ada lovelace".into()));
    assert_eq!(updated.data()[0][1], Value::Bool(false));

    let deleted = run(
        &mut session,
        "DELETE FROM orders WHERE total < 10 RETURNING id",
    );
    assert_eq!(deleted.data(), &[vec![Value::Int4(12)]]);
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM orders"),
        vec!["2"]
    );
}

#[test]
fn insert_select_and_returning() {
    let mut session = seeded();
    run(&mut session, "CREATE TABLE archive (id INT, name TEXT)");
    let result = run(
        &mut session,
        "INSERT INTO archive SELECT id, name FROM users WHERE id < 3 RETURNING id",
    );
    assert_eq!(result.data().len(), 2);
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM archive"),
        vec!["2"]
    );
    assert_eq!(
        run(&mut session, "INSERT INTO archive VALUES (9, 'x')").tag(),
        "INSERT 0 1"
    );
}

#[test]
fn constraints_are_enforced_with_postgres_messages() {
    let mut session = seeded();
    let error = expect_error(
        &mut session,
        "INSERT INTO users (id, name) VALUES (1, 'dup')",
    );
    assert_eq!(
        error.to_string(),
        "duplicate key value violates unique constraint \"users_pkey\""
    );
    assert_eq!(error.sqlstate(), "23505");

    let error = expect_error(
        &mut session,
        "INSERT INTO users (id, name) VALUES (4, NULL)",
    );
    assert_eq!(
        error.to_string(),
        "null value in column \"name\" of relation \"users\" violates not-null constraint"
    );
    assert_eq!(error.sqlstate(), "23502");

    let error = expect_error(
        &mut session,
        "INSERT INTO users (id, name, email) VALUES (4, 'e', 'ada@example.com')",
    );
    assert!(error.to_string().contains("unique constraint"), "{error}");
}

#[test]
fn planner_errors_look_like_postgres() {
    let mut session = seeded();
    assert_eq!(
        expect_error(&mut session, "SELECT nope FROM users").to_string(),
        "column \"nope\" does not exist"
    );
    assert_eq!(
        expect_error(&mut session, "SELECT id FROM users u, users v").to_string(),
        "column reference \"id\" is ambiguous"
    );
    assert_eq!(
        expect_error(&mut session, "SELECT * FROM missing").to_string(),
        "relation \"missing\" does not exist"
    );
    assert_eq!(
        expect_error(&mut session, "SELECT nope(1)").to_string(),
        "function nope(integer) does not exist"
    );
    assert_eq!(
        expect_error(
            &mut session,
            "SELECT * FROM users WHERE name LIKE 'a%' ESCAPE '!'"
        )
        .to_string(),
        "feature not supported: LIKE ... ESCAPE"
    );
}

#[test]
fn indexes_are_used_for_equality_predicates() {
    let mut session = seeded();
    let plan = text_rows(&mut session, "EXPLAIN SELECT name FROM users WHERE id = 2").join("\n");
    assert!(plan.contains("Index Scan on users"), "{plan}");
    assert!(plan.contains("Index Cond: (id = 2)"), "{plan}");
    assert!(plan.contains("Index Name: users_pkey"), "{plan}");
    let plan = text_rows(&mut session, "EXPLAIN SELECT * FROM users").join("\n");
    assert!(plan.contains("Seq Scan on users"), "{plan}");
}

#[test]
fn explain_analyze_reports_row_counts() {
    let mut session = seeded();
    let plan = text_rows(
        &mut session,
        "EXPLAIN ANALYZE SELECT count(*) FROM orders WHERE total > 1",
    )
    .join("\n");
    assert!(plan.contains("Aggregate"), "{plan}");
    assert!(plan.contains("actual"), "{plan}");
}

#[test]
fn create_index_and_unique_backfill() {
    let mut session = seeded();
    run(
        &mut session,
        "CREATE INDEX orders_user_idx ON orders (user_id)",
    );
    let plan = text_rows(
        &mut session,
        "EXPLAIN SELECT * FROM orders WHERE user_id = 1",
    )
    .join("\n");
    assert!(plan.contains("Index Scan on orders"), "{plan}");

    let error = expect_error(&mut session, "CREATE UNIQUE INDEX u ON orders (user_id)");
    assert!(matches!(error, ExecError::Storage(_)), "{error}");
}

#[test]
fn drop_truncate_and_transactions() {
    let mut session = seeded();
    run(&mut session, "TRUNCATE TABLE orders");
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM orders"),
        vec!["0"]
    );
    // The primary-key index was rebuilt, so re-inserting works.
    run(&mut session, "INSERT INTO orders VALUES (10, 1, 1.00)");

    assert_eq!(run(&mut session, "BEGIN").tag(), "BEGIN");
    assert!(session.in_transaction());
    assert_eq!(run(&mut session, "COMMIT").tag(), "COMMIT");
    assert!(!session.in_transaction());

    run(&mut session, "DROP TABLE orders");
    assert_eq!(
        expect_error(&mut session, "SELECT * FROM orders").to_string(),
        "relation \"orders\" does not exist"
    );
}

#[test]
fn views_are_resolved_through_the_planner() {
    let mut session = seeded();
    run(
        &mut session,
        "CREATE VIEW active_users AS SELECT id, name FROM users WHERE active",
    );
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM active_users"),
        vec!["3"]
    );
}

#[test]
fn type_system_integration() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE typed (
             a INT2, b INT8, c FLOAT8, d NUMERIC(8,2), e TEXT,
             f BOOL, g DATE, h TIMESTAMP, i UUID, j JSONB
         )",
    );
    run(
        &mut session,
        "INSERT INTO typed VALUES (
             1, 9000000000, 1.5, 12.34, 'text', true,
             '2024-02-29', '2024-02-29 13:05:00', '00000000-0000-0000-0000-000000000000', '{\"a\":1}'
         )",
    );
    assert_eq!(
        text_rows(&mut session, "SELECT a, b, c, d FROM typed"),
        vec!["1|9000000000|1.5|12.34"]
    );
    assert_eq!(
        text_rows(&mut session, "SELECT g, h FROM typed"),
        vec!["2024-02-29|2024-02-29 13:05:00"]
    );
    // A string literal is coerced to the column's type by the planner.
    assert_eq!(
        text_rows(&mut session, "SELECT a FROM typed WHERE a = '1'"),
        vec!["1"]
    );
    assert_eq!(
        text_rows(&mut session, "SELECT 1 + 2, 'a' || 'b', 7 / 2"),
        vec!["3|ab|3"]
    );
    let result = run(&mut session, "SELECT a, b FROM typed");
    assert_eq!(result.schema()[0].data_type, DataType::Int2);
    assert_eq!(result.schema()[1].data_type, DataType::Int8);
}

#[test]
fn multiple_statements_in_one_string() {
    let mut session = session();
    let results = session
        .execute("CREATE TABLE t (a INT); INSERT INTO t VALUES (1); SELECT a FROM t;")
        .unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[2].data(), &[vec![Value::Int4(1)]]);
}

#[test]
fn show_and_set_are_accepted() {
    let mut session = session();
    assert_eq!(run(&mut session, "SET search_path = public").tag(), "SET");
    let show = text_rows(&mut session, "SHOW search_path");
    assert_eq!(show, vec!["public"]);
    assert!(text_rows(&mut session, "SHOW server_version")[0].starts_with("15.0"));
    assert!(expect_error(&mut session, "SHOW nope")
        .to_string()
        .contains("unrecognized"));
}

/// Seeds a table whose columns are the types an untyped value has to adopt.
fn typed_events() -> Session {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE events (
             id INT PRIMARY KEY,
             at TIMESTAMPTZ,
             on_day DATE,
             ok BOOL,
             tag UUID,
             waited INTERVAL
         )",
    );
    run(
        &mut session,
        "INSERT INTO events VALUES
             (1, '2026-10-01 00:00:00+00', '2026-10-01', true,
                 '6f1e5a2c-0000-0000-0000-000000000001', '1 hour'),
             (2, '2025-01-01 00:00:00+00', '2025-01-01', false,
                 '6f1e5a2c-0000-0000-0000-000000000002', '2 hours')",
    );
    session
}

#[test]
fn untyped_string_literals_adopt_the_other_operands_type() {
    let mut session = typed_events();
    // PostgreSQL types a bare string literal as `unknown` and reads it as
    // whatever it is compared against; without this the engine answered
    // `operator does not exist: timestamp with time zone > text`.
    let count = |session: &mut Session, predicate: &str| {
        text_rows(
            session,
            &format!("SELECT count(*) FROM events WHERE {predicate}"),
        )
        .remove(0)
    };
    assert_eq!(count(&mut session, "at > '2026-01-01'"), "1");
    assert_eq!(count(&mut session, "on_day < '2026-01-01'"), "1");
    assert_eq!(count(&mut session, "ok = 'true'"), "1");
    assert_eq!(
        count(&mut session, "tag = '6f1e5a2c-0000-0000-0000-000000000002'"),
        "1"
    );
    assert_eq!(count(&mut session, "waited > '90 minutes'"), "1");
    // A value that cannot be read as the column's type is an input error, not a
    // missing operator.
    let error = expect_error(
        &mut session,
        "SELECT * FROM events WHERE at > 'not a timestamp'",
    );
    assert!(
        error.to_string().contains("invalid input syntax"),
        "{error}"
    );
}

#[test]
fn a_quoted_name_that_is_no_column_is_a_string() {
    // Clients written for MySQL quote their string literals with double quotes
    // (`WHERE type = "allowRegister"`), which PostgreSQL can only read as a
    // column reference. The name is therefore read as a literal when it matches
    // no column — but only when it is quoted *and* unqualified, so
    // `"settings"."type"` stays a reference and a typo stays an error.
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE settings (id INT, type TEXT, value TEXT)",
    );
    run(
        &mut session,
        "INSERT INTO settings VALUES (1, 'allowRegister', 'true'), (2, 'allowLogin', 'false')",
    );

    assert_eq!(
        text_rows(
            &mut session,
            "SELECT id FROM settings WHERE (\"settings\".\"type\" = \"allowRegister\")"
        ),
        vec!["1"]
    );
    // A real column still wins over the literal reading, quoted or not.
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT count(*) FROM settings WHERE \"type\" = \"allowLogin\""
        ),
        vec!["1"]
    );
    // ... and the fallback works where no relation is in scope at all.
    assert_eq!(text_rows(&mut session, "SELECT \"hello\""), vec!["hello"]);

    // A qualified name is never a literal: PostgreSQL's behaviour is kept.
    let error = expect_error(
        &mut session,
        "SELECT * FROM settings WHERE settings.\"nope\" = 1",
    );
    assert_eq!(
        error.to_string(),
        "column \"settings.nope\" does not exist",
        "{error}"
    );
    // Neither is an unquoted one, so a misspelled column cannot silently turn
    // into a string.
    let error = expect_error(&mut session, "SELECT * FROM settings WHERE typo = 1");
    assert_eq!(
        error.to_string(),
        "column \"typo\" does not exist",
        "{error}"
    );
}

#[test]
fn bind_parameters_are_read_as_the_columns_types() {
    let mut session = typed_events();
    let text = |value: &str| Value::Text(value.to_string());
    let ids = |session: &mut Session, sql: &str, params: &[Value]| {
        let results = session
            .execute_with_params(sql, params)
            .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
        let QueryResult::Rows { rows, .. } = &results[0] else {
            panic!("expected rows from `{sql}`");
        };
        rows.iter()
            .map(|row| row[0].clone())
            .collect::<Vec<Value>>()
    };

    // A driver sends every parameter in text format (there is no binary format
    // here), so each one is read as the type it is compared against.
    assert_eq!(
        ids(
            &mut session,
            "SELECT id FROM events WHERE ok = $1",
            &[text("false")]
        ),
        vec![Value::Int4(2)]
    );
    // `2026-10-01 04:00:00+08:00` is `2026-09-30 20:00Z`, so the first event is
    // *after* the bound instant — the offset has to be applied, not ignored.
    assert_eq!(
        ids(
            &mut session,
            "SELECT id FROM events WHERE at > $1",
            &[text("2026-10-01 04:00:00+08:00")]
        ),
        vec![Value::Int4(1)]
    );
    // A parameter without a zone is read in UTC, the session's zone.
    assert_eq!(
        ids(
            &mut session,
            "SELECT id FROM events WHERE at < $1",
            &[text("2026-01-01 00:00:00")]
        ),
        vec![Value::Int4(2)]
    );
    // `$1` also has to drive an index probe with the column's own type: an
    // untyped text key would compare against a different type tag and match
    // nothing, which is a wrong answer rather than an error.
    assert_eq!(
        ids(
            &mut session,
            "SELECT id FROM events WHERE id = $1",
            &[text("2")]
        ),
        vec![Value::Int4(2)]
    );

    // The same holds for a write: `INSERT ... RETURNING` with text parameters is
    // what an ORM's `Create` sends, and every value is cast to its column.
    let inserted = session
        .execute_with_params(
            "INSERT INTO events (id, at, on_day, ok) VALUES ($1, $2, $3, $4) RETURNING id, at",
            &[
                text("3"),
                text("2026-12-31 23:00:00+08:00"),
                text("2026-12-31"),
                text("true"),
            ],
        )
        .expect("parameterised insert");
    let QueryResult::Rows { rows, .. } = &inserted[0] else {
        panic!("expected RETURNING rows");
    };
    assert_eq!(rows[0][0], Value::Int4(3));
    assert!(matches!(rows[0][1], Value::Timestamptz(_)));
}
