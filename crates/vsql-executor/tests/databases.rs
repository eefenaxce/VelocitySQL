//! Multi-database behaviour.
//!
//! A VelocitySQL process is a *cluster*: `CREATE DATABASE` creates a namespace
//! whose relations are unreachable from any other database, and a session is
//! attached to exactly one of them for its whole lifetime — the same model, and
//! the same error messages, as PostgreSQL.

use std::sync::Arc;

use vsql_executor::{Engine, ExecError, QueryResult, Session};

fn engine() -> Arc<Engine> {
    Arc::new(Engine::new())
}

fn run(session: &mut Session, sql: &str) -> QueryResult {
    let mut results = session
        .execute(sql)
        .unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
    assert_eq!(results.len(), 1, "expected one result for `{sql}`");
    results.remove(0)
}

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
        Ok(_) => panic!("`{sql}` unexpectedly succeeded"),
        Err(error) => error,
    }
}

#[test]
fn a_fresh_cluster_has_the_default_and_compat_databases() {
    let engine = engine();
    let names: Vec<String> = engine
        .databases()
        .iter()
        .map(|database| database.name().to_string())
        .collect();
    assert!(names.contains(&"velocitysql".to_string()));
    assert!(names.contains(&"postgres".to_string()), "{names:?}");
    assert_eq!(Session::new(engine).database_name(), "velocitysql");
}

#[test]
fn databases_are_isolated() {
    let engine = engine();
    let mut control = Session::new(engine.clone());
    run(&mut control, "CREATE DATABASE shop");

    let mut shop = Session::with_database(engine.clone(), "shop").unwrap();
    run(
        &mut shop,
        "CREATE TABLE items (id INT PRIMARY KEY, name TEXT NOT NULL)",
    );
    run(&mut shop, "INSERT INTO items VALUES (1, 'widget')");

    // The same name in another database is a different relation with a
    // different shape.
    run(&mut control, "CREATE TABLE items (id INT PRIMARY KEY)");
    assert_eq!(
        failure(&mut control, "SELECT name FROM items").to_string(),
        "column \"name\" does not exist"
    );
    assert_eq!(
        text_rows(&mut control, "SELECT count(*) FROM items"),
        vec!["0"]
    );
    assert_eq!(
        text_rows(&mut shop, "SELECT count(*) FROM items"),
        vec!["1"]
    );
    assert_eq!(
        text_rows(&mut shop, "SELECT name FROM items"),
        vec!["widget"]
    );
}

#[test]
fn a_missing_database_reports_3d000() {
    let engine = engine();
    let error = Session::with_database(engine.clone(), "nope").unwrap_err();
    assert_eq!(error.to_string(), "database \"nope\" does not exist");
    assert_eq!(error.sqlstate(), "3D000");
}

#[test]
fn create_database_is_idempotent_with_if_not_exists() {
    let mut session = Session::new(engine());
    run(&mut session, "CREATE DATABASE shop");
    run(&mut session, "CREATE DATABASE IF NOT EXISTS shop");

    let error = failure(&mut session, "CREATE DATABASE shop");
    assert_eq!(error.to_string(), "database \"shop\" already exists");
    assert_eq!(error.sqlstate(), "42P04");
}

#[test]
fn names_are_folded_and_validated() {
    let engine = engine();
    let mut session = Session::new(engine.clone());
    // Quoted identifiers keep their spelling in general, but database names are
    // folded so that a connection string always finds them.
    run(&mut session, "CREATE DATABASE \"Shop\"");
    assert!(Session::with_database(engine.clone(), "shop").is_ok());
    assert!(Session::with_database(engine, "SHOP").is_ok());

    let error = failure(&mut session, "CREATE DATABASE pg_reserved");
    assert_eq!(
        error.to_string(),
        "unacceptable database name \"pg_reserved\""
    );
    assert_eq!(error.sqlstate(), "42939");

    let long = "x".repeat(64);
    let error = failure(&mut session, &format!("CREATE DATABASE {long}"));
    assert_eq!(error.sqlstate(), "42622", "{error}");
}

#[test]
fn drop_database_if_exists_is_idempotent() {
    let engine = engine();
    let mut session = Session::new(engine.clone());
    run(&mut session, "CREATE DATABASE doomed");
    run(&mut session, "DROP DATABASE doomed");
    run(&mut session, "DROP DATABASE IF EXISTS doomed");
    assert!(Session::with_database(engine.clone(), "doomed")
        .unwrap_err()
        .to_string()
        .contains("does not exist"));
}

#[test]
fn the_open_database_cannot_be_dropped() {
    let engine = engine();
    let mut session = Session::new(engine.clone());
    run(&mut session, "CREATE DATABASE shop");

    let error = failure(&mut session, "DROP DATABASE velocitysql");
    assert_eq!(error.to_string(), "cannot drop the currently open database");
    assert_eq!(error.sqlstate(), "55006");
}

#[test]
fn a_database_in_use_cannot_be_dropped() {
    let engine = engine();
    let mut control = Session::new(engine.clone());
    run(&mut control, "CREATE DATABASE shop");

    let attached = Session::with_database(engine.clone(), "shop").unwrap();
    let error = failure(&mut control, "DROP DATABASE shop");
    assert_eq!(
        error.to_string(),
        "database \"shop\" is being accessed by other users"
    );
    assert_eq!(error.sqlstate(), "55006");

    // Disconnecting releases the slot, which `Session::drop` is responsible for.
    drop(attached);
    run(&mut control, "DROP DATABASE shop");
    assert!(Session::with_database(engine, "shop").is_err());
}

#[test]
fn database_ddl_is_refused_inside_a_transaction() {
    let mut session = Session::new(engine());
    run(&mut session, "BEGIN");

    let error = failure(&mut session, "CREATE DATABASE other");
    assert_eq!(
        error.to_string(),
        "CREATE DATABASE cannot run inside a transaction block"
    );
    assert_eq!(error.sqlstate(), "25001");

    let error = failure(&mut session, "DROP DATABASE postgres");
    assert_eq!(
        error.to_string(),
        "DROP DATABASE cannot run inside a transaction block"
    );

    run(&mut session, "ROLLBACK");
    run(&mut session, "CREATE DATABASE other");
}

#[test]
fn current_database_follows_the_connection() {
    let engine = engine();
    let mut session = Session::new(engine.clone());
    assert_eq!(
        text_rows(&mut session, "SELECT current_database()"),
        vec!["velocitysql"]
    );

    run(&mut session, "CREATE DATABASE shop");
    session.switch_database("shop").unwrap();
    assert_eq!(session.database_name(), "shop");
    assert_eq!(
        text_rows(&mut session, "SELECT current_database()"),
        vec!["shop"]
    );

    session.switch_database("postgres").unwrap();
    assert_eq!(
        text_rows(&mut session, "SELECT current_database()"),
        vec!["postgres"]
    );
}

#[test]
fn switching_databases_changes_what_is_visible() {
    let engine = engine();
    let mut session = Session::new(engine.clone());
    run(&mut session, "CREATE DATABASE shop");
    run(&mut session, "CREATE TABLE here_only (a INT)");
    run(&mut session, "INSERT INTO here_only VALUES (7)");

    session.switch_database("shop").unwrap();
    assert_eq!(
        failure(&mut session, "SELECT * FROM here_only").to_string(),
        "relation \"here_only\" does not exist"
    );
    run(&mut session, "CREATE TABLE shop_only (a INT)");
    run(&mut session, "INSERT INTO shop_only VALUES (9)");

    session.switch_database("velocitysql").unwrap();
    assert_eq!(
        text_rows(&mut session, "SELECT a FROM here_only"),
        vec!["7"]
    );
    assert_eq!(
        failure(&mut session, "SELECT * FROM shop_only").to_string(),
        "relation \"shop_only\" does not exist"
    );

    // Switching to an unknown database leaves the session where it was.
    assert!(session.switch_database("nope").is_err());
    assert_eq!(session.database_name(), "velocitysql");
    assert_eq!(
        text_rows(&mut session, "SELECT a FROM here_only"),
        vec!["7"]
    );
}

#[test]
fn views_are_per_database() {
    let engine = engine();
    let mut control = Session::new(engine.clone());
    run(&mut control, "CREATE DATABASE shop");
    run(&mut control, "CREATE TABLE t (a INT)");
    run(&mut control, "CREATE VIEW v AS SELECT a FROM t");

    let mut shop = Session::with_database(engine, "shop").unwrap();
    run(&mut shop, "CREATE TABLE t (a INT)");
    assert_eq!(
        failure(&mut shop, "SELECT * FROM v").to_string(),
        "relation \"v\" does not exist"
    );
    assert_eq!(text_rows(&mut control, "SELECT count(*) FROM v"), vec!["0"]);
}

#[test]
fn schema_qualified_names_still_work_inside_a_database() {
    let engine = engine();
    let mut session = Session::new(engine);
    run(&mut session, "CREATE DATABASE shop");
    session.switch_database("shop").unwrap();
    run(&mut session, "CREATE SCHEMA app");
    run(&mut session, "CREATE TABLE app.t (a INT)");
    run(&mut session, "INSERT INTO app.t VALUES (1)");
    assert_eq!(text_rows(&mut session, "SELECT a FROM app.t"), vec!["1"]);
    assert_eq!(
        failure(&mut session, "SELECT * FROM t").to_string(),
        "relation \"t\" does not exist",
        "an unqualified name only searches public, like PostgreSQL's search_path"
    );
}
