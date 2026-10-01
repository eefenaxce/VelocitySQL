//! `CREATE TABLE` constraint end-to-end tests.
//!
//! Each test states the PostgreSQL behaviour it pins down. The interesting
//! cases here are the ones a driver's migration runner hits: a column-level
//! `REFERENCES` clause is a one-column `FOREIGN KEY`, a `SERIAL` column owns a
//! sequence that advances on every insert, and a `CHECK` constraint rejects
//! the first row that fails it.

use std::sync::Arc;

use vsql_executor::{Engine, ExecError, QueryResult, Session};
use vsql_types::Value;

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

/// Runs `sql` and returns the error it must produce.
fn expect_error(session: &mut Session, sql: &str) -> ExecError {
    match session.execute(sql) {
        Ok(_) => panic!("`{sql}` unexpectedly succeeded"),
        Err(error) => error,
    }
}

#[test]
fn column_level_references_becomes_a_foreign_key() {
    let mut session = session();
    run(&mut session, "CREATE TABLE users (id SERIAL PRIMARY KEY)");
    run(
        &mut session,
        "CREATE TABLE traffic (
             id SERIAL PRIMARY KEY,
             user_id INT NOT NULL REFERENCES users(id)
         )",
    );
    // `pg_constraint` reports the reference the way PostgreSQL does: the
    // referencing column against the referenced table's primary key, with the
    // NO ACTION actions a plain `REFERENCES` implies.
    assert_eq!(
        rows(
            &mut session,
            "SELECT contype, conkey, confkey, confupdtype, confmatchtype
               FROM pg_constraint
              WHERE conrelid = (SELECT oid FROM pg_class WHERE relname = 'traffic')
                AND contype = 'f'"
        ),
        [[
            Value::Text("f".to_string()),
            Value::Text("{2}".to_string()),
            Value::Text("{1}".to_string()),
            Value::Text("a".to_string()),
            Value::Text("s".to_string()),
        ]]
    );
}

#[test]
fn serial_columns_advance_an_owned_sequence() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (id SERIAL PRIMARY KEY, tag TEXT)",
    );
    run(&mut session, "INSERT INTO t (tag) VALUES ('a')");
    run(&mut session, "INSERT INTO t (tag) VALUES ('b')");
    assert_eq!(
        rows(&mut session, "SELECT id FROM t ORDER BY id"),
        [[Value::Int4(1)], [Value::Int4(2)]]
    );
    // The default is a real `nextval` call over the sequence PostgreSQL would
    // have named, and `pg_get_serial_sequence` finds it.
    assert_eq!(
        rows(&mut session, "SELECT pg_get_serial_sequence('t', 'id')"),
        [[Value::Text("public.t_id_seq".to_string())]]
    );
    // An explicit id bypasses the default; the sequence keeps counting from
    // where it is, exactly as PostgreSQL's sequence does - it never looks at
    // the table.
    run(&mut session, "INSERT INTO t VALUES (10, 'c')");
    run(&mut session, "INSERT INTO t (tag) VALUES ('d')");
    assert_eq!(
        rows(&mut session, "SELECT id FROM t WHERE tag = 'd'"),
        [[Value::Int4(3)]]
    );
}

#[test]
fn check_constraint_rejects_the_first_violating_row() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (
             id SERIAL PRIMARY KEY,
             h INT NOT NULL DEFAULT 0 CHECK (h >= 0 AND h <= 23)
         )",
    );
    run(&mut session, "INSERT INTO t DEFAULT VALUES");
    // NULL is UNKNOWN: a check constraint passes when it cannot prove the row
    // wrong.
    run(&mut session, "CREATE TABLE optional (h INT CHECK (h < 10))");
    run(&mut session, "INSERT INTO optional VALUES (NULL)");
    let error = expect_error(&mut session, "INSERT INTO t (h) VALUES (24)");
    assert_eq!(error.sqlstate(), "23514", "{error}");
    assert_eq!(
        error.to_string(),
        "new row for relation \"t\" violates check constraint \"t_h_check\""
    );
}

#[test]
fn declared_constraint_names_are_kept() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (
             id SERIAL PRIMARY KEY,
             tag TEXT CONSTRAINT tag_len CHECK (length(tag) < 5),
             uid INT CONSTRAINT uid_fk REFERENCES t (id)
         )",
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT conname, contype FROM pg_constraint
              WHERE conrelid = (SELECT oid FROM pg_class WHERE relname = 't')
                AND contype IN ('c', 'f')
              ORDER BY conname"
        ),
        [
            [
                Value::Text("tag_len".to_string()),
                Value::Text("c".to_string())
            ],
            [
                Value::Text("uid_fk".to_string()),
                Value::Text("f".to_string())
            ],
        ]
    );
    // The violating row is reported against the declared name.
    let error = expect_error(&mut session, "INSERT INTO t (tag) VALUES ('toolong')");
    assert_eq!(
        error.to_string(),
        "new row for relation \"t\" violates check constraint \"tag_len\""
    );
}

#[test]
fn foreign_key_type_and_arity_mismatches_are_refused() {
    let mut session = session();
    run(&mut session, "CREATE TABLE a (id INT PRIMARY KEY)");
    let error = expect_error(&mut session, "CREATE TABLE b (x TEXT REFERENCES a (id))");
    assert_eq!(error.sqlstate(), "XX000", "{error}");
    assert!(error.to_string().contains("incompatible types"), "{error}");
}
