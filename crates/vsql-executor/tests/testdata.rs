//! Loads the SQL under `testdata/`.
//!
//! The sample data is documentation: a reader is told to run these files to get
//! a database worth querying. Loading them here keeps that promise honest — an
//! engine change that breaks a sample fails this suite instead of the reader.

use std::sync::Arc;

use vsql_executor::{Engine, ExecError, QueryResult, Session};

const SEED: &str = include_str!("../../../testdata/seed.sql");
const CLUSTER: &str = include_str!("../../../testdata/cluster.sql");
const ANALYTICS: &str = include_str!("../../../testdata/analytics.sql");

fn session() -> Session {
    Session::new(Arc::new(Engine::new()))
}

fn run(session: &mut Session, sql: &str) -> QueryResult {
    let mut results = session
        .execute(sql)
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
    assert_eq!(results.len(), 1, "expected one result for `{sql}`");
    results.remove(0)
}

/// Every row rendered as `a|b`, which keeps the assertions readable.
fn text_rows(session: &mut Session, sql: &str) -> Vec<String> {
    run(session, sql)
        .data()
        .iter()
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

fn failure(session: &mut Session, sql: &str) -> ExecError {
    match session.execute(sql) {
        Ok(_) => panic!("`{sql}` should have been refused"),
        Err(error) => error,
    }
}

#[test]
fn the_storefront_sample_loads() {
    let mut session = session();
    session.execute(SEED).expect("seed.sql must load");

    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM shop.customers"),
        ["8"]
    );
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM shop.products"),
        ["12"]
    );
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM shop.orders"),
        ["10"]
    );
    assert_eq!(
        text_rows(&mut session, "SELECT count(*) FROM shop.order_lines"),
        ["20"]
    );
}

#[test]
fn the_storefront_sample_answers_the_queries_it_exists_for() {
    let mut session = session();
    session.execute(SEED).expect("seed.sql must load");

    // The view is the join-and-sum the schema is built around.
    let totals = text_rows(
        &mut session,
        "SELECT order_id, total FROM shop.order_totals ORDER BY order_id",
    );
    assert_eq!(totals.len(), 10);
    assert_eq!(totals.first().map(String::as_str), Some("1|114.90"));
    assert_eq!(totals.last().map(String::as_str), Some("10|138.00"));

    // A lookup on one indexed column and a grouping on another.
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT count(*) FROM shop.products WHERE in_stock = 0"
        ),
        ["3"]
    );
    assert_eq!(
        text_rows(
            &mut session,
            "SELECT category, count(*) FROM shop.products GROUP BY category ORDER BY category"
        ),
        ["cable|3", "desk|1", "display|3", "input|5"]
    );

    // The view joined back to the table it belongs to.
    let spend = text_rows(
        &mut session,
        "SELECT c.full_name, sum(t.total) AS spent
           FROM shop.customers c
           JOIN shop.order_totals t ON t.customer_id = c.id
          GROUP BY c.full_name
          ORDER BY c.full_name",
    );
    assert_eq!(spend.len(), 7, "one customer never ordered");
    assert_eq!(spend[0], "Ada Lovelace|488.90");
    assert_eq!(spend[4], "Katherine Johnson|649.00");
}

#[test]
fn the_storefront_sample_is_protected_by_its_constraints() {
    let mut session = session();
    session.execute(SEED).expect("seed.sql must load");

    let duplicate = failure(
        &mut session,
        "INSERT INTO shop.customers (email, full_name, signed_up)
         VALUES ('ada@example.com', 'Impostor', '2026-08-01 00:00:00+00')",
    );
    assert_eq!(
        duplicate.to_string(),
        "duplicate key value violates unique constraint \"customers_email_key\""
    );
    assert_eq!(duplicate.sqlstate(), "23505");

    // `REFERENCES` is declared, and `pg_constraint` reports it, but nothing
    // enforces it yet; only the CHECK below is rejected.
    let credit = failure(
        &mut session,
        "UPDATE shop.customers SET credit = -1 WHERE id = 1",
    );
    assert_eq!(
        credit.to_string(),
        "new row for relation \"customers\" violates check constraint \"customers_credit_check\""
    );
    assert_eq!(credit.sqlstate(), "23514");
}

#[test]
fn the_warehouse_sample_loads_in_its_own_database() {
    let engine = Arc::new(Engine::new());
    let mut admin = Session::new(engine.clone());
    admin.execute(CLUSTER).expect("cluster.sql must load");
    assert!(
        engine.database("analytics").is_some(),
        "the database it creates"
    );
    assert!(engine.role("app").is_some(), "the role it creates");

    // A session is bound to one database, so the warehouse is loaded through
    // one that is connected to `analytics` as `app`.
    let mut warehouse = Session::connect(engine, "analytics", "app").expect("connect");
    warehouse
        .execute(ANALYTICS)
        .expect("analytics.sql must load");

    assert_eq!(
        text_rows(&mut warehouse, "SELECT count(*) FROM events"),
        ["16"]
    );
    assert_eq!(
        text_rows(&mut warehouse, "SELECT count(*) FROM daily_rollup"),
        ["9"]
    );
    assert_eq!(
        text_rows(
            &mut warehouse,
            "SELECT day, source, events FROM events_by_source
              WHERE source = 'partner' ORDER BY day"
        ),
        ["2026-07-03|partner|2", "2026-07-04|partner|1"]
    );

    // The composite key refuses a second rollup for the same day and source.
    let duplicate = failure(
        &mut warehouse,
        "INSERT INTO daily_rollup (day, source, event_count) VALUES ('2026-07-01', 'web', 0)",
    );
    assert_eq!(duplicate.sqlstate(), "23505");

    // A database is a separate catalog: neither the storefront's schema nor its
    // tables are visible here.
    let missing_schema = failure(&mut warehouse, "SELECT count(*) FROM shop.customers");
    assert_eq!(missing_schema.sqlstate(), "3F000");
    let missing_table = failure(&mut warehouse, "SELECT count(*) FROM order_lines");
    assert_eq!(missing_table.sqlstate(), "42P01");

    // `app` is an ordinary role, so cluster-wide DDL is refused.
    let denied = failure(&mut warehouse, "CREATE DATABASE elsewhere");
    assert_eq!(denied.sqlstate(), "42501");
}
