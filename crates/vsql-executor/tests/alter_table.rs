//! `ALTER TABLE` end-to-end tests.
//!
//! Each test states the PostgreSQL behaviour it pins down, because that is the
//! only specification this engine has. The interesting cases are the ones where
//! the *stored rows* have to change with the definition: a column that is added
//! takes a value in every existing row, and a column whose type changes has to
//! bring its values along.

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

/// A two-row table, so every `ALTER TABLE` has something to do.
fn table() -> Session {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)",
    );
    run(&mut session, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");
    session
}

#[test]
fn add_column_fills_existing_rows_with_the_default() {
    let mut session = table();
    run(
        &mut session,
        "ALTER TABLE t ADD COLUMN active BOOL DEFAULT true",
    );

    assert_eq!(
        rows(&mut session, "SELECT id, active FROM t ORDER BY id"),
        [
            vec![Value::Int4(1), Value::Bool(true)],
            vec![Value::Int4(2), Value::Bool(true)],
        ]
    );
    // The new column is the last one, so the ordinals the rows already have - and
    // therefore the primary key index - keep their meaning.
    assert_eq!(
        rows(&mut session, "SELECT name FROM t WHERE id = 2"),
        [[Value::Text("b".to_string())]]
    );
}

#[test]
fn add_column_without_a_default_leaves_the_existing_rows_null() {
    let mut session = table();
    run(&mut session, "ALTER TABLE t ADD COLUMN note TEXT");

    assert_eq!(
        rows(&mut session, "SELECT note FROM t ORDER BY id"),
        [[Value::Null], [Value::Null]]
    );
    run(
        &mut session,
        "INSERT INTO t (id, name, note) VALUES (3, 'c', 'hi')",
    );
    assert_eq!(
        rows(&mut session, "SELECT note FROM t WHERE id = 3"),
        [[Value::Text("hi".to_string())]]
    );
}

#[test]
fn add_column_not_null_needs_a_default_when_the_table_has_rows() {
    let mut session = table();
    let error = expect_error(&mut session, "ALTER TABLE t ADD COLUMN code TEXT NOT NULL");
    assert_eq!(error.sqlstate(), "23502", "{error}");
    // The statement is refused as a whole: no column is half added.
    let error = expect_error(&mut session, "SELECT code FROM t");
    assert_eq!(error.sqlstate(), "42703", "{error}");

    // With no rows to fill, the same statement is fine - PostgreSQL only refuses
    // when a row would end up violating the constraint.
    run(&mut session, "CREATE TABLE empty (id INT)");
    run(
        &mut session,
        "ALTER TABLE empty ADD COLUMN code TEXT NOT NULL",
    );
    run(
        &mut session,
        "INSERT INTO empty (id, code) VALUES (1, 'ok')",
    );
    assert_eq!(
        rows(&mut session, "SELECT code FROM empty"),
        [[Value::Text("ok".to_string())]]
    );
}

#[test]
fn add_column_if_not_exists_is_a_no_op() {
    let mut session = table();
    run(
        &mut session,
        "ALTER TABLE t ADD COLUMN IF NOT EXISTS name TEXT",
    );
    assert_eq!(
        rows(&mut session, "SELECT name FROM t ORDER BY id"),
        [
            vec![Value::Text("a".to_string())],
            vec![Value::Text("b".to_string())],
        ],
        "the column that was already there keeps its values"
    );
}

#[test]
fn add_column_rejects_a_name_that_is_already_taken() {
    let mut session = table();
    let error = expect_error(&mut session, "ALTER TABLE t ADD COLUMN name TEXT");
    assert_eq!(error.sqlstate(), "42701", "{error}");

    // A dropped column keeps its slot, but not its name.
    run(&mut session, "ALTER TABLE t DROP COLUMN name");
    run(&mut session, "ALTER TABLE t ADD COLUMN name TEXT");
    assert_eq!(
        rows(&mut session, "SELECT name FROM t ORDER BY id"),
        [[Value::Null], [Value::Null]],
        "the new column starts empty rather than reusing the dropped values"
    );
}

#[test]
fn drop_column_hides_the_column_and_keeps_the_table_writable() {
    let mut session = table();
    run(&mut session, "ALTER TABLE t DROP COLUMN name");

    assert_eq!(rows(&mut session, "SELECT * FROM t ORDER BY id").len(), 2);
    assert_eq!(
        rows(
            &mut session,
            "SELECT column_name FROM information_schema.columns
               WHERE table_name = 't' ORDER BY ordinal_position"
        ),
        [[Value::Text("id".to_string())]]
    );
    let error = expect_error(&mut session, "SELECT name FROM t");
    assert_eq!(error.sqlstate(), "42703", "{error}");

    run(&mut session, "INSERT INTO t (id) VALUES (3)");
    assert_eq!(
        rows(&mut session, "SELECT id FROM t ORDER BY id").len(),
        3,
        "a row that omits the dropped column still has the right arity"
    );
}

#[test]
fn drop_column_if_exists_is_a_no_op_for_a_missing_column() {
    let mut session = table();
    run(&mut session, "ALTER TABLE t DROP COLUMN IF EXISTS nothing");
    let error = expect_error(&mut session, "ALTER TABLE t DROP COLUMN nothing");
    assert_eq!(error.sqlstate(), "42703", "{error}");
}

#[test]
fn drop_column_drops_the_index_that_used_it() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY, code TEXT UNIQUE)",
    );
    run(&mut session, "INSERT INTO t VALUES (1, 'x')");
    run(&mut session, "ALTER TABLE t DROP COLUMN code");

    assert_eq!(
        rows(
            &mut session,
            "SELECT relname FROM pg_class WHERE relname = 't_code_key'"
        ),
        Vec::<Vec<Value>>::new(),
        "the unique index went with the column"
    );
    // Which means a second row with the same (now gone) value is accepted.
    run(&mut session, "INSERT INTO t (id) VALUES (2)");
    assert_eq!(rows(&mut session, "SELECT id FROM t ORDER BY id").len(), 2);
}

#[test]
fn drop_column_takes_the_primary_key_with_it() {
    let mut session = table();
    run(&mut session, "ALTER TABLE t DROP COLUMN id");

    assert_eq!(
        rows(
            &mut session,
            "SELECT relname FROM pg_class WHERE relname = 't_pkey'"
        ),
        Vec::<Vec<Value>>::new()
    );
    // With no primary key left, the table keeps taking rows.
    run(&mut session, "INSERT INTO t (name) VALUES ('c')");
    assert_eq!(rows(&mut session, "SELECT name FROM t").len(), 3);
}

#[test]
fn rename_column_and_table() {
    let mut session = table();
    run(&mut session, "ALTER TABLE t RENAME COLUMN name TO label");
    assert_eq!(
        rows(&mut session, "SELECT label FROM t ORDER BY id"),
        [
            vec![Value::Text("a".to_string())],
            vec![Value::Text("b".to_string())],
        ]
    );

    run(&mut session, "ALTER TABLE t RENAME TO people");
    assert_eq!(
        rows(&mut session, "SELECT id FROM people ORDER BY id").len(),
        2
    );
    let error = expect_error(&mut session, "SELECT id FROM t");
    assert_eq!(error.sqlstate(), "42P01", "{error}");

    // An index keeps the name it was created with, as in PostgreSQL.
    assert_eq!(
        rows(
            &mut session,
            "SELECT relname FROM pg_class WHERE relname = 't_pkey'"
        )
        .len(),
        1
    );
}

#[test]
fn rename_affects_the_rest_of_the_same_statement() {
    let mut session = table();
    run(
        &mut session,
        "ALTER TABLE t RENAME TO people, RENAME COLUMN name TO label",
    );
    assert_eq!(
        rows(&mut session, "SELECT label FROM people ORDER BY id"),
        [
            vec![Value::Text("a".to_string())],
            vec![Value::Text("b".to_string())],
        ]
    );
}

#[test]
fn rename_table_refuses_a_name_that_is_taken() {
    let mut session = table();
    run(&mut session, "CREATE TABLE other (id INT)");
    let error = expect_error(&mut session, "ALTER TABLE t RENAME TO other");
    assert_eq!(error.sqlstate(), "42P07", "{error}");
}

#[test]
fn alter_column_type_converts_the_stored_values() {
    let mut session = table();
    run(&mut session, "ALTER TABLE t ALTER COLUMN id TYPE BIGINT");

    assert_eq!(
        rows(&mut session, "SELECT id FROM t ORDER BY id"),
        [vec![Value::Int8(1)], vec![Value::Int8(2)]],
        "the values were converted, not reinterpreted"
    );
    // The index was rebuilt on the converted values, so a lookup still finds them.
    assert_eq!(
        rows(&mut session, "SELECT name FROM t WHERE id = 2"),
        [[Value::Text("b".to_string())]]
    );
    run(&mut session, "INSERT INTO t (id, name) VALUES (3, 'c')");
    assert_eq!(rows(&mut session, "SELECT id FROM t").len(), 3);
}

#[test]
fn alter_column_type_refuses_values_it_cannot_hold() {
    let mut session = table();
    let error = expect_error(&mut session, "ALTER TABLE t ALTER COLUMN name TYPE INT");
    assert_eq!(error.sqlstate(), "22P02", "{error}");

    // All or nothing: the type is unchanged and the values are still readable.
    assert_eq!(
        rows(&mut session, "SELECT name FROM t ORDER BY id"),
        [
            vec![Value::Text("a".to_string())],
            vec![Value::Text("b".to_string())],
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT data_type FROM information_schema.columns
               WHERE table_name = 't' AND column_name = 'name'"
        ),
        [[Value::Text("text".to_string())]]
    );
}

#[test]
fn set_default_applies_to_later_inserts_only() {
    let mut session = table();
    run(
        &mut session,
        "ALTER TABLE t ALTER COLUMN name SET DEFAULT 'anon'",
    );
    run(&mut session, "INSERT INTO t (id) VALUES (3)");
    assert_eq!(
        rows(&mut session, "SELECT name FROM t WHERE id = 3"),
        [[Value::Text("anon".to_string())]]
    );
    assert_eq!(
        rows(&mut session, "SELECT name FROM t WHERE id = 1"),
        [[Value::Text("a".to_string())]],
        "the default never rewrites a value that is already stored"
    );

    run(&mut session, "ALTER TABLE t ALTER COLUMN name DROP DEFAULT");
    run(&mut session, "INSERT INTO t (id) VALUES (4)");
    assert_eq!(
        rows(&mut session, "SELECT name FROM t WHERE id = 4"),
        [[Value::Null]]
    );
}

#[test]
fn set_not_null_is_refused_while_a_row_violates_it() {
    let mut session = session();
    run(&mut session, "CREATE TABLE t (id INT)");
    run(&mut session, "INSERT INTO t VALUES (1), (NULL)");

    let error = expect_error(&mut session, "ALTER TABLE t ALTER COLUMN id SET NOT NULL");
    assert_eq!(error.sqlstate(), "23502", "{error}");

    run(&mut session, "DELETE FROM t WHERE id IS NULL");
    run(&mut session, "ALTER TABLE t ALTER COLUMN id SET NOT NULL");
    let error = expect_error(&mut session, "INSERT INTO t VALUES (NULL)");
    assert_eq!(error.sqlstate(), "23502", "{error}");

    run(&mut session, "ALTER TABLE t ALTER COLUMN id DROP NOT NULL");
    run(&mut session, "INSERT INTO t VALUES (NULL)");
    assert_eq!(rows(&mut session, "SELECT id FROM t").len(), 2);
}

#[test]
fn add_primary_key_constraint_backs_it_with_an_index() {
    let mut session = session();
    run(&mut session, "CREATE TABLE t (id INT, name TEXT)");
    run(&mut session, "INSERT INTO t VALUES (1, 'a')");
    run(&mut session, "ALTER TABLE t ADD PRIMARY KEY (id)");

    assert_eq!(
        rows(
            &mut session,
            "SELECT relname FROM pg_class WHERE relname = 't_pkey'"
        )
        .len(),
        1
    );
    let error = expect_error(&mut session, "INSERT INTO t VALUES (1, 'duplicate')");
    assert_eq!(error.sqlstate(), "23505", "{error}");

    // A second primary key is what PostgreSQL refuses to install.
    let error = expect_error(&mut session, "ALTER TABLE t ADD PRIMARY KEY (name)");
    assert!(
        error.to_string().contains("multiple primary keys"),
        "{error}"
    );
}

#[test]
fn add_unique_constraint_rejects_data_that_already_violates_it() {
    let mut session = session();
    run(&mut session, "CREATE TABLE t (id INT, name TEXT)");
    run(
        &mut session,
        "INSERT INTO t VALUES (1, 'same'), (2, 'same')",
    );

    let error = expect_error(
        &mut session,
        "ALTER TABLE t ADD CONSTRAINT t_name_key UNIQUE (name)",
    );
    assert_eq!(error.sqlstate(), "23505", "{error}");
    assert_eq!(
        rows(
            &mut session,
            "SELECT relname FROM pg_class WHERE relname = 't_name_key'"
        ),
        Vec::<Vec<Value>>::new(),
        "a constraint that could not be built leaves no index behind"
    );

    run(&mut session, "DELETE FROM t WHERE id = 2");
    run(
        &mut session,
        "ALTER TABLE t ADD CONSTRAINT t_name_key UNIQUE (name)",
    );
    let error = expect_error(&mut session, "INSERT INTO t VALUES (3, 'same')");
    assert_eq!(error.sqlstate(), "23505", "{error}");
}

#[test]
fn add_check_constraint_is_validated_and_then_enforced() {
    let mut session = table();
    // The stored rows satisfy the constraint, so the install succeeds; the
    // next violating write is what gets rejected.
    run(&mut session, "ALTER TABLE t ADD CHECK (id > 0)");
    let error = expect_error(&mut session, "INSERT INTO t VALUES (-1, 'c')");
    assert_eq!(error.sqlstate(), "23514", "{error}");
}

#[test]
fn add_foreign_key_requires_an_existing_referenced_table() {
    let mut session = table();
    let error = expect_error(
        &mut session,
        "ALTER TABLE t ADD CONSTRAINT t_fk FOREIGN KEY (id) REFERENCES other (id)",
    );
    assert_eq!(error.sqlstate(), "42P01", "{error}");
}

#[test]
fn drop_constraint_removes_the_index() {
    let mut session = session();
    run(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY, code TEXT UNIQUE)",
    );
    run(&mut session, "INSERT INTO t VALUES (1, 'x')");
    run(&mut session, "ALTER TABLE t DROP CONSTRAINT t_code_key");

    assert_eq!(
        rows(
            &mut session,
            "SELECT relname FROM pg_class WHERE relname = 't_code_key'"
        ),
        Vec::<Vec<Value>>::new()
    );
    run(&mut session, "INSERT INTO t VALUES (2, 'x')");
    assert_eq!(rows(&mut session, "SELECT id FROM t ORDER BY id").len(), 2);

    let error = expect_error(&mut session, "ALTER TABLE t DROP CONSTRAINT t_code_key");
    assert_eq!(error.sqlstate(), "42704", "{error}");
    run(
        &mut session,
        "ALTER TABLE t DROP CONSTRAINT IF EXISTS t_code_key",
    );

    // Dropping the primary key constraint drops the primary key with it.
    run(&mut session, "ALTER TABLE t DROP CONSTRAINT t_pkey");
    run(&mut session, "INSERT INTO t VALUES (1, 'y')");
    assert_eq!(rows(&mut session, "SELECT id FROM t").len(), 3);
}

#[test]
fn owner_to_is_recorded_in_pg_tables() {
    let mut session = table();
    run(&mut session, "CREATE ROLE app");
    assert_eq!(
        rows(
            &mut session,
            "SELECT tableowner FROM pg_tables WHERE tablename = 't'"
        ),
        [[Value::Text("velocitysql".to_string())]],
        "everything the engine creates belongs to the bootstrap superuser"
    );

    run(&mut session, "ALTER TABLE t OWNER TO app");
    assert_eq!(
        rows(
            &mut session,
            "SELECT tableowner FROM pg_tables WHERE tablename = 't'"
        ),
        [[Value::Text("app".to_string())]]
    );
    // `pg_class.relowner` is the same role, addressed by OID.
    assert_eq!(
        rows(
            &mut session,
            "SELECT r.rolname FROM pg_class c JOIN pg_roles r ON r.oid = c.relowner
               WHERE c.relname = 't'"
        ),
        [[Value::Text("app".to_string())]]
    );

    let error = expect_error(&mut session, "ALTER TABLE t OWNER TO nobody");
    assert_eq!(error.sqlstate(), "42704", "{error}");
}

#[test]
fn alter_table_reports_missing_relations_and_views() {
    let mut session = table();
    let error = expect_error(&mut session, "ALTER TABLE missing ADD COLUMN x INT");
    assert_eq!(error.sqlstate(), "42P01", "{error}");

    run(&mut session, "CREATE VIEW v AS SELECT id FROM t");
    let error = expect_error(&mut session, "ALTER TABLE v ADD COLUMN x INT");
    assert_eq!(error.to_string(), "\"v\" is not a table", "{error}");

    let error = expect_error(&mut session, "ALTER TABLE t RENAME COLUMN nope TO x");
    assert_eq!(error.sqlstate(), "42703", "{error}");
    let error = expect_error(
        &mut session,
        "ALTER TABLE t ALTER COLUMN nope SET DEFAULT 1",
    );
    assert_eq!(error.sqlstate(), "42703", "{error}");
    let error = expect_error(&mut session, "ALTER TABLE t ALTER COLUMN nope TYPE BIGINT");
    assert_eq!(error.sqlstate(), "42703", "{error}");
}

#[test]
fn alter_table_reaches_a_table_through_its_schema() {
    let mut session = table();
    run(
        &mut session,
        "ALTER TABLE public.t ADD COLUMN active BOOL DEFAULT false",
    );
    run(
        &mut session,
        "ALTER TABLE public.t RENAME COLUMN name TO label",
    );
    assert_eq!(
        rows(&mut session, "SELECT label, active FROM t ORDER BY id"),
        [
            vec![Value::Text("a".to_string()), Value::Bool(false)],
            vec![Value::Text("b".to_string()), Value::Bool(false)],
        ]
    );
}
